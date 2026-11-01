//! Pairing codes and panel sessions.
//!
//! Only SHA-256 hashes of codes and session tokens reach the database, so a
//! leaked database file cannot be replayed as a login. Callers hash with
//! [`hash_secret`] before calling in.

use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::{Db, Result};

/// A paired phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// Public identifier, used to revoke from the panel. Not a credential.
    pub id: String,
    pub token_hash: String,
    pub csrf_token: String,
    pub device_name: String,
    pub created_unix: i64,
    pub last_seen_unix: i64,
    pub expires_unix: i64,
}

/// Hex SHA-256 of a secret (pairing code or session token).
pub fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

impl Db {
    /// Stores a pairing code (already hashed).
    pub fn insert_pairing_code(
        &self,
        code_hash: &str,
        created_unix: i64,
        expires_unix: i64,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO pairing_codes (code_hash, created_unix, expires_unix) VALUES (?1, ?2, ?3)",
            params![code_hash, created_unix, expires_unix],
        )?;
        Ok(())
    }

    /// Redeems a code: deletes it and returns true only if it existed and had
    /// not expired. A single statement, so two phones cannot both redeem it.
    pub fn take_pairing_code(&self, code_hash: &str, now_unix: i64) -> Result<bool> {
        let removed = self.lock()?.execute(
            "DELETE FROM pairing_codes WHERE code_hash = ?1 AND expires_unix > ?2",
            params![code_hash, now_unix],
        )?;
        Ok(removed == 1)
    }

    /// Removes expired codes; returns how many.
    pub fn purge_expired_pairing_codes(&self, now_unix: i64) -> Result<usize> {
        Ok(self.lock()?.execute(
            "DELETE FROM pairing_codes WHERE expires_unix <= ?1",
            params![now_unix],
        )?)
    }

    /// Invalidates every outstanding code (after too many wrong guesses).
    pub fn clear_pairing_codes(&self) -> Result<usize> {
        Ok(self.lock()?.execute("DELETE FROM pairing_codes", [])?)
    }

    pub fn count_pairing_codes(&self, now_unix: i64) -> Result<usize> {
        let n: i64 = self.lock()?.query_row(
            "SELECT count(*) FROM pairing_codes WHERE expires_unix > ?1",
            params![now_unix],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    pub fn insert_session(&self, s: &SessionRow) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO sessions (id, token_hash, csrf_token, device_name, created_unix,
                 last_seen_unix, expires_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                s.id,
                s.token_hash,
                s.csrf_token,
                s.device_name,
                s.created_unix,
                s.last_seen_unix,
                s.expires_unix
            ],
        )?;
        Ok(())
    }

    /// The live session for a token hash; expired sessions are not returned.
    pub fn find_session(&self, token_hash: &str, now_unix: i64) -> Result<Option<SessionRow>> {
        Ok(self
            .lock()?
            .query_row(
                &format!(
                    "SELECT {SESSION_COLS} FROM sessions WHERE token_hash = ?1 AND expires_unix > ?2"
                ),
                params![token_hash, now_unix],
                session_from_row,
            )
            .optional()?)
    }

    /// Slides the expiry forward after use.
    pub fn touch_session(
        &self,
        token_hash: &str,
        last_seen_unix: i64,
        expires_unix: i64,
    ) -> Result<()> {
        self.lock()?.execute(
            "UPDATE sessions SET last_seen_unix = ?2, expires_unix = ?3 WHERE token_hash = ?1",
            params![token_hash, last_seen_unix, expires_unix],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self, now_unix: i64) -> Result<Vec<SessionRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE expires_unix > ?1
             ORDER BY created_unix, id"
        ))?;
        let rows = stmt.query_map(params![now_unix], session_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Revokes a device by its public id.
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])?
            > 0)
    }

    pub fn purge_expired_sessions(&self, now_unix: i64) -> Result<usize> {
        Ok(self.lock()?.execute(
            "DELETE FROM sessions WHERE expires_unix <= ?1",
            params![now_unix],
        )?)
    }
}

const SESSION_COLS: &str =
    "id, token_hash, csrf_token, device_name, created_unix, last_seen_unix, expires_unix";

fn session_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        token_hash: r.get(1)?,
        csrf_token: r.get(2)?,
        device_name: r.get(3)?,
        created_unix: r.get(4)?,
        last_seen_unix: r.get(5)?,
        expires_unix: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn session(id: &str, token: &str, expires: i64) -> SessionRow {
        SessionRow {
            id: id.into(),
            token_hash: hash_secret(token),
            csrf_token: "csrf".into(),
            device_name: "Pixel".into(),
            created_unix: 100,
            last_seen_unix: 100,
            expires_unix: expires,
        }
    }

    #[test]
    fn hash_is_stable_and_hides_the_secret() {
        assert_eq!(hash_secret("abc"), hash_secret("abc"));
        assert_ne!(hash_secret("abc"), hash_secret("abd"));
        assert_eq!(hash_secret("abc").len(), 64);
        assert!(!hash_secret("abc").contains("abc"));
    }

    #[test]
    fn a_code_can_be_redeemed_exactly_once() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_pairing_code(&hash_secret("ABCD2345"), 0, 600)?;
        assert!(db.take_pairing_code(&hash_secret("ABCD2345"), 10)?);
        assert!(!db.take_pairing_code(&hash_secret("ABCD2345"), 11)?);
        Ok(())
    }

    #[test]
    fn expired_and_unknown_codes_are_refused() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_pairing_code(&hash_secret("ABCD2345"), 0, 600)?;
        assert!(
            !db.take_pairing_code(&hash_secret("ABCD2345"), 600)?,
            "expiry is exclusive"
        );
        assert!(!db.take_pairing_code(&hash_secret("ZZZZ2345"), 1)?);
        Ok(())
    }

    #[test]
    fn purge_and_clear_codes() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_pairing_code("a", 0, 100)?;
        db.insert_pairing_code("b", 0, 900)?;
        assert_eq!(db.count_pairing_codes(50)?, 2);
        assert_eq!(db.purge_expired_pairing_codes(500)?, 1);
        assert_eq!(db.clear_pairing_codes()?, 1);
        assert_eq!(db.count_pairing_codes(0)?, 0);
        Ok(())
    }

    #[test]
    fn sessions_are_found_by_token_hash_until_they_expire() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_session(&session("d1", "tok", 1_000))?;
        let hash = hash_secret("tok");
        assert_eq!(
            db.find_session(&hash, 500)?.map(|s| s.id),
            Some("d1".into())
        );
        assert!(db.find_session(&hash, 1_000)?.is_none(), "expired");
        assert!(db.find_session(&hash_secret("other"), 500)?.is_none());
        Ok(())
    }

    #[test]
    fn touching_slides_the_expiry() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_session(&session("d1", "tok", 1_000))?;
        let hash = hash_secret("tok");
        db.touch_session(&hash, 900, 5_000)?;
        let s = db.find_session(&hash, 2_000)?.ok_or("session gone")?;
        assert_eq!((s.last_seen_unix, s.expires_unix), (900, 5_000));
        Ok(())
    }

    #[test]
    fn revoking_a_device_ends_its_session_at_once() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_session(&session("d1", "t1", 1_000))?;
        db.insert_session(&session("d2", "t2", 1_000))?;
        assert!(db.delete_session("d1")?);
        assert!(!db.delete_session("d1")?);
        assert!(db.find_session(&hash_secret("t1"), 10)?.is_none());
        assert_eq!(db.list_sessions(10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn duplicate_token_hash_is_rejected() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_session(&session("d1", "same", 1_000))?;
        assert!(db.insert_session(&session("d2", "same", 1_000)).is_err());
        Ok(())
    }

    #[test]
    fn expired_sessions_are_purged() -> TestResult {
        let db = Db::open_in_memory()?;
        db.insert_session(&session("old", "a", 50))?;
        db.insert_session(&session("new", "b", 500))?;
        assert_eq!(db.purge_expired_sessions(100)?, 1);
        assert_eq!(db.list_sessions(0)?.len(), 1);
        Ok(())
    }
}
