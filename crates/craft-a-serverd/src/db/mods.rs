//! Mods installed from Modrinth (schema v4).

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{Db, Result};

/// One installed mod, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModRow {
    pub server_id: String,
    pub project_id: String,
    pub version_id: String,
    pub title: String,
    pub version_number: String,
    pub file_name: String,
    /// Lowercase hex SHA-512 of the jar as verified at install time.
    pub sha512: String,
    pub size_bytes: u64,
    /// Installed because another mod needed it, not because the user asked.
    pub as_dependency: bool,
    pub installed_unix: i64,
}

/// A row to write plus the projects it requires (for "who needs this").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModChange {
    pub row: ModRow,
    pub requires: Vec<String>,
}

const MOD_COLS: &str = "server_id, project_id, version_id, title, version_number, file_name,
    sha512, size_bytes, as_dependency, installed_unix";

fn mod_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ModRow> {
    let size: i64 = r.get(7)?;
    Ok(ModRow {
        server_id: r.get(0)?,
        project_id: r.get(1)?,
        version_id: r.get(2)?,
        title: r.get(3)?,
        version_number: r.get(4)?,
        file_name: r.get(5)?,
        sha512: r.get(6)?,
        size_bytes: u64::try_from(size).unwrap_or(0),
        as_dependency: r.get(8)?,
        installed_unix: r.get(9)?,
    })
}

impl Db {
    /// Installed mods of a server, by title.
    pub fn list_mods(&self, server_id: &str) -> Result<Vec<ModRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {MOD_COLS} FROM mods WHERE server_id = ?1 ORDER BY lower(title), project_id"
        ))?;
        let rows = stmt.query_map(params![server_id], mod_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Projects that `project_id` requires, as recorded when it was installed.
    pub fn mod_requirements(&self, server_id: &str, project_id: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT requires_project_id FROM mod_requires
             WHERE server_id = ?1 AND project_id = ?2 ORDER BY requires_project_id",
        )?;
        let rows = stmt.query_map(params![server_id, project_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Installed projects that require `project_id`.
    pub fn mod_dependents(&self, server_id: &str, project_id: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT project_id FROM mod_requires
             WHERE server_id = ?1 AND requires_project_id = ?2 ORDER BY project_id",
        )?;
        let rows = stmt.query_map(params![server_id, project_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Applies removals and upserts in one transaction, so the records change
    /// together or not at all. Safe to repeat (crash recovery does).
    pub fn apply_mod_changes(
        &self,
        server_id: &str,
        remove_projects: &[String],
        upserts: &[ModChange],
    ) -> Result<()> {
        let conn = self.lock()?;
        let tx = conn.unchecked_transaction()?;
        for project in remove_projects {
            tx.execute(
                "DELETE FROM mod_requires WHERE server_id = ?1 AND project_id = ?2",
                params![server_id, project],
            )?;
            tx.execute(
                "DELETE FROM mods WHERE server_id = ?1 AND project_id = ?2",
                params![server_id, project],
            )?;
        }
        for change in upserts {
            let r = &change.row;
            tx.execute(
                "DELETE FROM mod_requires WHERE server_id = ?1 AND project_id = ?2",
                params![server_id, r.project_id],
            )?;
            tx.execute(
                "INSERT OR REPLACE INTO mods (server_id, project_id, version_id, title,
                     version_number, file_name, sha512, size_bytes, as_dependency, installed_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    server_id,
                    r.project_id,
                    r.version_id,
                    r.title,
                    r.version_number,
                    r.file_name,
                    r.sha512,
                    r.size_bytes as i64,
                    r.as_dependency,
                    r.installed_unix
                ],
            )?;
            for required in &change.requires {
                tx.execute(
                    "INSERT OR IGNORE INTO mod_requires (server_id, project_id, requires_project_id)
                     VALUES (?1, ?2, ?3)",
                    params![server_id, r.project_id, required],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn sample_row(server_id: &str, project_id: &str, dep: bool) -> ModRow {
    ModRow {
        server_id: server_id.into(),
        project_id: project_id.into(),
        version_id: format!("v{project_id}"),
        title: format!("Mod {project_id}"),
        version_number: "1.0.0".into(),
        file_name: format!("{project_id}.jar"),
        sha512: "ab".repeat(64),
        size_bytes: 10,
        as_dependency: dep,
        installed_unix: 5,
    }
}

#[cfg(test)]
mod tests {
    use craft_a_server_core::model::ServerState;

    use super::*;
    use crate::db::migrations::MIGRATIONS;
    use crate::db::{ServerRow, configure, migrate};

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn db_with_server() -> std::result::Result<Db, Box<dyn std::error::Error>> {
        let db = Db::open_in_memory()?;
        db.insert_server(&ServerRow {
            id: "a".into(),
            name: "n".into(),
            mc_version: "26.3".into(),
            state: ServerState::Stopped,
            host_port: 25565,
            memory_max_mib: None,
            rcon_password: "x".into(),
            eula_accepted_at: 1,
            created_unix: 1,
            auto_start: false,
        })?;
        Ok(db)
    }

    fn change(project: &str, dep: bool, requires: &[&str]) -> ModChange {
        ModChange {
            row: sample_row("a", project, dep),
            requires: requires.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn records_roundtrip_with_dependents() -> TestResult {
        let db = db_with_server()?;
        db.apply_mod_changes(
            "a",
            &[],
            &[change("dep", true, &[]), change("root", false, &["dep"])],
        )?;
        let rows = db.list_mods("a")?;
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|r| r.project_id == "dep" && r.as_dependency)
        );
        assert_eq!(db.mod_dependents("a", "dep")?, ["root"]);
        assert_eq!(db.mod_requirements("a", "root")?, ["dep"]);
        assert!(db.mod_dependents("a", "root")?.is_empty());
        Ok(())
    }

    #[test]
    fn changes_are_idempotent_and_replace() -> TestResult {
        let db = db_with_server()?;
        let c = change("root", false, &["dep"]);
        db.apply_mod_changes("a", &[], std::slice::from_ref(&c))?;
        db.apply_mod_changes("a", &[], &[c])?;
        assert_eq!(db.list_mods("a")?.len(), 1);
        assert_eq!(db.mod_requirements("a", "root")?, ["dep"]);
        // An update replaces the row and its requirements.
        let mut newer = change("root", false, &["other"]);
        newer.row.version_id = "v2".into();
        db.apply_mod_changes("a", &[], &[newer])?;
        let rows = db.list_mods("a")?;
        assert_eq!(rows.first().map(|r| r.version_id.as_str()), Some("v2"));
        assert_eq!(db.mod_requirements("a", "root")?, ["other"]);
        Ok(())
    }

    #[test]
    fn removal_drops_requirements_and_server_delete_cascades() -> TestResult {
        let db = db_with_server()?;
        db.apply_mod_changes("a", &[], &[change("root", false, &["dep"])])?;
        db.apply_mod_changes("a", &["root".to_owned()], &[])?;
        assert!(db.list_mods("a")?.is_empty());
        assert!(db.mod_requirements("a", "root")?.is_empty());

        db.apply_mod_changes("a", &[], &[change("root", false, &["dep"])])?;
        assert!(db.delete_server("a")?);
        assert!(db.list_mods("a")?.is_empty());
        assert!(db.mod_dependents("a", "dep")?.is_empty());
        Ok(())
    }

    #[test]
    fn a_failing_batch_changes_nothing() -> TestResult {
        let db = db_with_server()?;
        db.apply_mod_changes("a", &[], &[change("keep", false, &[])])?;
        // The second upsert is refused by a trigger: the removal before it rolls back too.
        db.lock()?.execute_batch(
            "CREATE TRIGGER refuse BEFORE INSERT ON mods WHEN NEW.project_id = 'bad'
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )?;
        let result = db.apply_mod_changes(
            "a",
            &["keep".to_owned()],
            &[change("fine", false, &[]), change("bad", false, &[])],
        );
        assert!(result.is_err());
        let rows = db.list_mods("a")?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows.first().map(|r| r.project_id.as_str()), Some("keep"));
        Ok(())
    }

    #[test]
    fn migration_v4_keeps_the_copy_of_the_old_file() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("craft.db");
        {
            let conn = rusqlite::Connection::open(&path)?;
            configure(&conn)?;
            migrate(&conn, Some(&path), MIGRATIONS.get(..3).unwrap_or_default())?;
            conn.execute(
                "INSERT INTO servers (id, name, mc_version, state, host_port, rcon_password,
                     eula_accepted_at, created_unix) VALUES ('a','n','26.3','stopped',25565,'x',1,1)",
                [],
            )?;
        }
        let db = Db::open(&path)?;
        assert_eq!(db.schema_version()?, MIGRATIONS.len());
        assert!(dir.path().join("craft.db.pre-v3.bak").exists());
        assert!(db.get_server("a")?.is_some());
        assert!(db.list_mods("a")?.is_empty());
        Ok(())
    }
}
