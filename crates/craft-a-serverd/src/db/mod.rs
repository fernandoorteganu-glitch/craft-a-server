//! SQLite storage: servers, backups and an audit log of events.
//!
//! The API is synchronous (one connection behind a mutex); async callers use
//! `spawn_blocking`. The file runs in WAL mode so a power cut leaves a
//! consistent database, and every schema change is a numbered migration with a
//! copy of the old file taken first.

mod migrations;
mod pairing;

pub use pairing::{SessionRow, hash_secret};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use craft_a_server_core::model::ServerState;
use rusqlite::{Connection, OptionalExtension, params};

use migrations::MIGRATIONS;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not back up the database before migrating: {0}")]
    PreMigrationCopy(#[source] std::io::Error),
    #[error("database is from a newer version (schema {found}, this build knows {known})")]
    SchemaTooNew { found: usize, known: usize },
    #[error("stored data is invalid: {0}")]
    Corrupt(String),
    #[error("no such record: {0}")]
    NotFound(String),
    #[error("database lock poisoned")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, DbError>;

/// A server as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRow {
    pub id: String,
    pub name: String,
    pub mc_version: String,
    pub state: ServerState,
    pub host_port: u16,
    pub memory_max_mib: Option<u64>,
    pub rcon_password: String,
    pub eula_accepted_at: i64,
    pub created_unix: i64,
    /// The user wants this server running; it is started again after a reboot.
    pub auto_start: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRow {
    pub id: String,
    pub server_id: String,
    pub kind: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub created_unix: i64,
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub id: i64,
    pub ts_unix: i64,
    pub server_id: Option<String>,
    pub kind: String,
    pub detail: String,
}

/// Handle to the database; cheap to clone.
#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Opens (creating if needed) the database file, migrating it to the latest schema.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        configure(&conn)?;
        migrate(&conn, Some(path), MIGRATIONS)?;
        Ok(Db::from_conn(conn))
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate(&conn, None, MIGRATIONS)?;
        Ok(Db::from_conn(conn))
    }

    fn from_conn(conn: Connection) -> Db {
        Db {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| DbError::Poisoned)
    }

    /// Schema version of the open database.
    pub fn schema_version(&self) -> Result<usize> {
        let conn = self.lock()?;
        Ok(user_version(&conn)?)
    }

    pub fn insert_server(&self, s: &ServerRow) -> Result<()> {
        let (state, reason) = s.state.to_parts();
        self.lock()?.execute(
            "INSERT INTO servers (id, name, mc_version, state, state_reason, host_port,
                 memory_max_mib, rcon_password, eula_accepted_at, created_unix, auto_start)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                s.id,
                s.name,
                s.mc_version,
                state,
                reason,
                s.host_port,
                s.memory_max_mib.map(|v| v as i64),
                s.rcon_password,
                s.eula_accepted_at,
                s.created_unix,
                s.auto_start
            ],
        )?;
        Ok(())
    }

    pub fn get_server(&self, id: &str) -> Result<Option<ServerRow>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                &format!("SELECT {SERVER_COLS} FROM servers WHERE id = ?1"),
                params![id],
                server_from_row,
            )
            .optional()?;
        row.transpose()
    }

    pub fn list_servers(&self) -> Result<Vec<ServerRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SERVER_COLS} FROM servers ORDER BY created_unix, id"
        ))?;
        let rows = stmt.query_map([], server_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn set_state(&self, id: &str, state: &ServerState) -> Result<()> {
        let (name, reason) = state.to_parts();
        let changed = self.lock()?.execute(
            "UPDATE servers SET state = ?2, state_reason = ?3 WHERE id = ?1",
            params![id, name, reason],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(format!("server {id}")));
        }
        Ok(())
    }

    pub fn set_auto_start(&self, id: &str, auto_start: bool) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE servers SET auto_start = ?2 WHERE id = ?1",
            params![id, auto_start],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(format!("server {id}")));
        }
        Ok(())
    }

    pub fn delete_server(&self, id: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .execute("DELETE FROM servers WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn insert_backup(&self, b: &BackupRow) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO backups (id, server_id, kind, file_name, size_bytes, sha256,
                 created_unix, verified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                b.id,
                b.server_id,
                b.kind,
                b.file_name,
                b.size_bytes as i64,
                b.sha256,
                b.created_unix,
                b.verified
            ],
        )?;
        Ok(())
    }

    pub fn set_backup_verified(&self, id: &str, verified: bool) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE backups SET verified = ?2 WHERE id = ?1",
            params![id, verified],
        )?;
        if changed == 0 {
            return Err(DbError::NotFound(format!("backup {id}")));
        }
        Ok(())
    }

    pub fn get_backup(&self, id: &str) -> Result<Option<BackupRow>> {
        Ok(self
            .lock()?
            .query_row(
                &format!("SELECT {BACKUP_COLS} FROM backups WHERE id = ?1"),
                params![id],
                backup_from_row,
            )
            .optional()?)
    }

    /// Backups of a server, newest first.
    pub fn list_backups(&self, server_id: &str) -> Result<Vec<BackupRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {BACKUP_COLS} FROM backups WHERE server_id = ?1
             ORDER BY created_unix DESC, id DESC"
        ))?;
        let rows = stmt.query_map(params![server_id], backup_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn delete_backup(&self, id: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .execute("DELETE FROM backups WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn record_event(
        &self,
        ts_unix: i64,
        server_id: Option<&str>,
        kind: &str,
        detail: &str,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO events (ts_unix, server_id, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            params![ts_unix, server_id, kind, detail],
        )?;
        Ok(())
    }

    /// Most recent events first.
    pub fn recent_events(&self, limit: u32) -> Result<Vec<EventRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, ts_unix, server_id, kind, detail FROM events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(EventRow {
                id: r.get(0)?,
                ts_unix: r.get(1)?,
                server_id: r.get(2)?,
                kind: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

const SERVER_COLS: &str = "id, name, mc_version, state, state_reason, host_port, memory_max_mib,
    rcon_password, eula_accepted_at, created_unix, auto_start";
const BACKUP_COLS: &str =
    "id, server_id, kind, file_name, size_bytes, sha256, created_unix, verified";

fn server_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<ServerRow>> {
    let state_name: String = r.get(3)?;
    let reason: Option<String> = r.get(4)?;
    let Some(state) = ServerState::from_parts(&state_name, reason.as_deref()) else {
        return Ok(Err(DbError::Corrupt(format!(
            "unknown state '{state_name}'"
        ))));
    };
    let port: i64 = r.get(5)?;
    let Ok(host_port) = u16::try_from(port) else {
        return Ok(Err(DbError::Corrupt(format!("bad port {port}"))));
    };
    let mem: Option<i64> = r.get(6)?;
    Ok(Ok(ServerRow {
        id: r.get(0)?,
        name: r.get(1)?,
        mc_version: r.get(2)?,
        state,
        host_port,
        memory_max_mib: mem.and_then(|m| u64::try_from(m).ok()),
        rcon_password: r.get(7)?,
        eula_accepted_at: r.get(8)?,
        created_unix: r.get(9)?,
        auto_start: r.get(10)?,
    }))
}

fn backup_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BackupRow> {
    let size: i64 = r.get(4)?;
    Ok(BackupRow {
        id: r.get(0)?,
        server_id: r.get(1)?,
        kind: r.get(2)?,
        file_name: r.get(3)?,
        size_bytes: u64::try_from(size).unwrap_or(0),
        sha256: r.get(5)?,
        created_unix: r.get(6)?,
        verified: r.get(7)?,
    })
}

fn configure(conn: &Connection) -> Result<()> {
    // `journal_mode` returns a row, so it must be read, not executed.
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

fn user_version(conn: &Connection) -> rusqlite::Result<usize> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(usize::try_from(v).unwrap_or(0))
}

fn migrate(conn: &Connection, path: Option<&Path>, migrations: &[&str]) -> Result<()> {
    let current = user_version(conn)?;
    let known = migrations.len();
    if current > known {
        return Err(DbError::SchemaTooNew {
            found: current,
            known,
        });
    }
    if current == known {
        return Ok(());
    }
    if let (Some(path), true) = (path, current > 0) {
        copy_before_migration(conn, path, current)?;
    }
    for (index, sql) in migrations.iter().enumerate().skip(current) {
        // Each migration and its version bump commit together or not at all.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
        tx.commit()?;
    }
    Ok(())
}

fn copy_before_migration(conn: &Connection, path: &Path, from_version: usize) -> Result<()> {
    // Fold the WAL into the main file so a plain file copy is complete.
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    let mut backup: PathBuf = path.into();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "db".into());
    backup.set_file_name(format!("{name}.pre-v{from_version}.bak"));
    std::fs::copy(path, backup).map_err(DbError::PreMigrationCopy)?;
    Ok(())
}

#[cfg(test)]
mod tests;
