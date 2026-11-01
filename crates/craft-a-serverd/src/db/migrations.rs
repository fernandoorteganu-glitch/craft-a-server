//! Versioned schema migrations, applied in order and tracked with
//! `PRAGMA user_version`. Migrations are forward-only; to keep rolling back the
//! daemon binary safe, a migration may only *add* things (expand), never drop
//! or rename what the previous release reads (contract comes a release later).

/// `MIGRATIONS[n]` upgrades the schema from version `n` to `n + 1`.
pub const MIGRATIONS: &[&str] = &[
    // v1: initial schema
    r#"
    CREATE TABLE servers (
        id               TEXT PRIMARY KEY,
        name             TEXT NOT NULL,
        mc_version       TEXT NOT NULL,
        state            TEXT NOT NULL,
        state_reason     TEXT,
        host_port        INTEGER NOT NULL,
        memory_max_mib   INTEGER,
        rcon_password    TEXT NOT NULL,
        eula_accepted_at INTEGER NOT NULL,
        created_unix     INTEGER NOT NULL
    );
    CREATE TABLE backups (
        id           TEXT PRIMARY KEY,
        server_id    TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        kind         TEXT NOT NULL,
        file_name    TEXT NOT NULL,
        size_bytes   INTEGER NOT NULL,
        sha256       TEXT NOT NULL,
        created_unix INTEGER NOT NULL,
        verified     INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX backups_by_server ON backups(server_id, created_unix);
    CREATE TABLE events (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        ts_unix   INTEGER NOT NULL,
        server_id TEXT,
        kind      TEXT NOT NULL,
        detail    TEXT NOT NULL
    );
    "#,
    // v2: remember whether the user wants a server running, so it can come
    // back by itself after a reboot or power cut.
    "ALTER TABLE servers ADD COLUMN auto_start INTEGER NOT NULL DEFAULT 0;",
    // v3: phone pairing. Only hashes of codes and session tokens are stored.
    r#"
    CREATE TABLE pairing_codes (
        code_hash    TEXT PRIMARY KEY,
        created_unix INTEGER NOT NULL,
        expires_unix INTEGER NOT NULL
    );
    CREATE TABLE sessions (
        id             TEXT PRIMARY KEY,
        token_hash     TEXT NOT NULL UNIQUE,
        csrf_token     TEXT NOT NULL,
        device_name    TEXT NOT NULL,
        created_unix   INTEGER NOT NULL,
        last_seen_unix INTEGER NOT NULL,
        expires_unix   INTEGER NOT NULL
    );
    "#,
];
