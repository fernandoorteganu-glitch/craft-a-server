use super::*;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn server(id: &str, created: i64) -> ServerRow {
    ServerRow {
        id: id.into(),
        name: format!("Server {id}"),
        mc_version: "26.3".into(),
        state: ServerState::Creating,
        host_port: 25565,
        memory_max_mib: Some(4096),
        rcon_password: "secret".into(),
        eula_accepted_at: 1_000,
        created_unix: created,
        auto_start: false,
    }
}

fn backup(id: &str, server_id: &str, created: i64) -> BackupRow {
    BackupRow {
        id: id.into(),
        server_id: server_id.into(),
        kind: "daily".into(),
        file_name: format!("{id}.tar.zst"),
        size_bytes: 123,
        sha256: "ab".repeat(32),
        created_unix: created,
        verified: false,
    }
}

#[test]
fn migrates_fresh_database() -> TestResult {
    let db = Db::open_in_memory()?;
    assert_eq!(db.schema_version()?, MIGRATIONS.len());
    Ok(())
}

#[test]
fn server_roundtrip_and_state_changes() -> TestResult {
    let db = Db::open_in_memory()?;
    db.insert_server(&server("a", 1))?;
    assert_eq!(db.get_server("a")?, Some(server("a", 1)));
    assert_eq!(db.get_server("missing")?, None);

    db.set_state("a", &ServerState::Failed("boom".into()))?;
    let got = db.get_server("a")?.ok_or("server vanished")?;
    assert_eq!(got.state, ServerState::Failed("boom".into()));

    assert!(matches!(
        db.set_state("missing", &ServerState::Stopped),
        Err(DbError::NotFound(_))
    ));
    Ok(())
}

#[test]
fn duplicate_server_id_is_rejected() -> TestResult {
    let db = Db::open_in_memory()?;
    db.insert_server(&server("a", 1))?;
    assert!(db.insert_server(&server("a", 2)).is_err());
    Ok(())
}

#[test]
fn list_servers_is_ordered() -> TestResult {
    let db = Db::open_in_memory()?;
    db.insert_server(&server("b", 2))?;
    db.insert_server(&server("a", 1))?;
    let ids: Vec<_> = db.list_servers()?.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, ["a", "b"]);
    Ok(())
}

#[test]
fn backups_are_listed_newest_first_and_cascade_on_server_delete() -> TestResult {
    let db = Db::open_in_memory()?;
    db.insert_server(&server("a", 1))?;
    db.insert_backup(&backup("b1", "a", 10))?;
    db.insert_backup(&backup("b2", "a", 20))?;
    let ids: Vec<_> = db.list_backups("a")?.into_iter().map(|b| b.id).collect();
    assert_eq!(ids, ["b2", "b1"]);

    db.set_backup_verified("b1", true)?;
    assert!(db.get_backup("b1")?.ok_or("missing")?.verified);

    assert!(db.delete_server("a")?);
    assert!(db.list_backups("a")?.is_empty());
    Ok(())
}

#[test]
fn backup_requires_existing_server() -> TestResult {
    let db = Db::open_in_memory()?;
    assert!(db.insert_backup(&backup("b1", "nope", 1)).is_err());
    Ok(())
}

#[test]
fn events_are_returned_newest_first() -> TestResult {
    let db = Db::open_in_memory()?;
    db.record_event(1, Some("a"), "state", "creating -> stopped")?;
    db.record_event(2, None, "boot", "daemon started")?;
    let events = db.recent_events(10)?;
    assert_eq!(events.len(), 2);
    assert_eq!(events.first().map(|e| e.kind.as_str()), Some("boot"));
    assert_eq!(db.recent_events(1)?.len(), 1);
    Ok(())
}

#[test]
fn corrupt_state_is_reported_not_panicked() -> TestResult {
    let db = Db::open_in_memory()?;
    db.insert_server(&server("a", 1))?;
    db.lock()?
        .execute("UPDATE servers SET state = 'bogus' WHERE id = 'a'", [])?;
    assert!(matches!(db.get_server("a"), Err(DbError::Corrupt(_))));
    Ok(())
}

#[test]
fn file_database_persists_uses_wal_and_reopens() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("craft.db");
    {
        let db = Db::open(&path)?;
        db.insert_server(&server("a", 1))?;
        let mode: String = db
            .lock()?
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        assert_eq!(mode, "wal");
    }
    let db = Db::open(&path)?;
    assert!(db.get_server("a")?.is_some());
    // Already current: no pre-migration copy is made.
    assert!(!dir.path().join("craft.db.pre-v1.bak").exists());
    Ok(())
}

#[test]
fn newer_schema_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("craft.db");
    {
        let conn = Connection::open(&path)?;
        conn.execute_batch("PRAGMA user_version = 99")?;
    }
    assert!(matches!(
        Db::open(&path),
        Err(DbError::SchemaTooNew { found: 99, .. })
    ));
    Ok(())
}

#[test]
fn upgrade_copies_the_file_first_and_applies_new_migrations() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("craft.db");
    let v1: &[&str] = &["CREATE TABLE a (x INTEGER); INSERT INTO a VALUES (1);"];
    let v2: &[&str] = &[
        "CREATE TABLE a (x INTEGER); INSERT INTO a VALUES (1);",
        "CREATE TABLE b (y INTEGER);",
    ];
    {
        let conn = Connection::open(&path)?;
        configure(&conn)?;
        migrate(&conn, Some(&path), v1)?;
        assert!(!dir.path().join("craft.db.pre-v1.bak").exists());
    }
    let conn = Connection::open(&path)?;
    configure(&conn)?;
    migrate(&conn, Some(&path), v2)?;
    assert_eq!(user_version(&conn)?, 2);
    let old = Connection::open(dir.path().join("craft.db.pre-v1.bak"))?;
    assert_eq!(user_version(&old)?, 1);
    Ok(())
}

#[test]
fn failed_migration_rolls_back_and_keeps_old_version() -> TestResult {
    let conn = Connection::open_in_memory()?;
    let migrations: &[&str] = &[
        "CREATE TABLE a (x INTEGER);",
        "CREATE TABLE b (y INTEGER); THIS IS NOT SQL;",
    ];
    assert!(migrate(&conn, None, migrations).is_err());
    assert_eq!(user_version(&conn)?, 1);
    let b: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name = 'b'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(b, 0);
    Ok(())
}
