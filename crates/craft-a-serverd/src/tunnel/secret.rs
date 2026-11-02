//! The agent's secret key: where it lives and how it is read.
//!
//! The playit agent writes the secret itself (`--secret-path`, mode 0600)
//! when the user accepts the claim. This module never creates it, never logs
//! it and never puts it in a command line: it only reads it to authenticate
//! the address lookup, and tightens its permissions if they are too wide.

use std::io;
use std::path::Path;

/// Largest secret file we read.
const MAX_SECRET_FILE_BYTES: u64 = 4096;

/// A secret key. `Debug` and `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<hidden>)")
    }
}

impl Secret {
    /// Accepts a hex string of 16 to 256 characters.
    pub fn new(hex: &str) -> Option<Secret> {
        let hex = hex.trim();
        let valid = (16..=256).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit());
        valid.then(|| Secret(hex.to_owned()))
    }

    /// Value of the `Authorization` header for the playit API (scheme
    /// unverified against a live account; see the tunnel docs).
    pub fn authorization(&self) -> String {
        format!("agent-key {}", self.0)
    }

    /// Exposed for tests that check the secret does not leak.
    #[cfg(test)]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("the agent's secret file is not a regular file")]
    NotRegular,
    #[error("the agent's secret file is too large")]
    TooLarge,
    #[error("the agent's secret file is not in a known format")]
    Invalid,
    #[error("could not read the agent's secret file: {0}")]
    Io(#[from] io::Error),
}

/// Parses `secret_key = "<hex>"` (TOML) or a bare hex string.
pub fn parse_secret_file(text: &str) -> Option<Secret> {
    let text = text.trim();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("secret_key") {
            let value = rest.trim_start().strip_prefix('=')?.trim();
            return Secret::new(value.trim_matches(['"', '\'']));
        }
    }
    if text.lines().count() == 1 {
        return Secret::new(text);
    }
    None
}

/// Reads the secret if the file exists. `Ok(None)` means "not claimed yet".
///
/// A secret file readable by others is fixed to 0600 (the agent already
/// creates it that way; this is defence in depth).
pub fn read_secret(path: &Path) -> Result<Option<Secret>, SecretError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() {
        return Err(SecretError::NotRegular);
    }
    if meta.len() > MAX_SECRET_FILE_BYTES {
        return Err(SecretError::TooLarge);
    }
    if meta.len() == 0 {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let text = std::fs::read_to_string(path)?;
    parse_secret_file(&text)
        .map(Some)
        .ok_or(SecretError::Invalid)
}

/// Whether a secret file exists and has content (a claim was completed).
pub fn secret_present(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const HEX: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn debug_never_shows_the_secret() -> TestResult {
        let s = Secret::new(HEX).ok_or("secret")?;
        assert!(!format!("{s:?}").contains(HEX));
        assert!(s.authorization().starts_with("agent-key "));
        Ok(())
    }

    #[test]
    fn accepts_toml_and_bare_hex_and_nothing_else() -> TestResult {
        assert_eq!(
            parse_secret_file(&format!("secret_key = \"{HEX}\"\n")).ok_or("toml")?,
            Secret::new(HEX).ok_or("hex")?
        );
        assert!(parse_secret_file(&format!("{HEX}\n")).is_some());
        for bad in [
            "",
            "secret_key = \"not hex\"",
            "secret_key = ",
            "other = \"0123456789abcdef0123456789abcdef\"\nmore",
            "abc",
            "0123456789abcdef0123456789abcdeg",
            &"a".repeat(300),
        ] {
            assert!(parse_secret_file(bad).is_none(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn missing_and_empty_files_mean_not_claimed() -> TestResult {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("playit.toml");
        assert!(read_secret(&p)?.is_none());
        assert!(!secret_present(&p));
        std::fs::write(&p, b"")?;
        assert!(read_secret(&p)?.is_none());
        assert!(!secret_present(&p));
        std::fs::write(&p, format!("secret_key = \"{HEX}\"\n"))?;
        assert!(read_secret(&p)?.is_some());
        assert!(secret_present(&p));
        Ok(())
    }

    #[test]
    fn garbage_and_oversize_files_are_errors() -> TestResult {
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("playit.toml");
        std::fs::write(&p, b"hello world, not a key")?;
        assert!(matches!(read_secret(&p), Err(SecretError::Invalid)));
        std::fs::write(&p, vec![b'a'; 5000])?;
        assert!(matches!(read_secret(&p), Err(SecretError::TooLarge)));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_wide_secret_file_is_tightened_and_symlinks_are_refused() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("playit.toml");
        std::fs::write(&p, format!("secret_key = \"{HEX}\"\n"))?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644))?;
        assert!(read_secret(&p)?.is_some());
        assert_eq!(std::fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);

        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&p, &link)?;
        assert!(matches!(read_secret(&link), Err(SecretError::NotRegular)));
        Ok(())
    }
}
