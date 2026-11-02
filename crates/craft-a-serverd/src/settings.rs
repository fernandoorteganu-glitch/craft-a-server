//! Per-server settings the user can change, kept in `settings.json` next to
//! (not inside) the server's data directory.
//!
//! A separate file instead of a database column on purpose: the database
//! schema is forward-only, and a new column would stop an older release from
//! opening the file after `install.sh --rollback`. An older release simply
//! ignores this file. The data directory is not used because it belongs to the
//! game container and goes into backups.
//!
//! Reading never fails the caller: a missing, unreadable or out-of-range file
//! means "use the defaults".

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use craft_a_server_core::settings::{
    VIEW_DISTANCE_DEFAULT, VIEW_DISTANCE_MAX, VIEW_DISTANCE_MIN, valid_view_distance,
};

const FILE_NAME: &str = "settings.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSettings {
    /// `None` keeps the game's default.
    #[serde(default)]
    pub view_distance: Option<u8>,
}

fn path(server_dir: &Path) -> PathBuf {
    server_dir.join(FILE_NAME)
}

/// The settings of a server; defaults when the file is missing or unusable.
pub fn load(server_dir: &Path) -> ServerSettings {
    let file = path(server_dir);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return ServerSettings::default(),
        Err(e) => {
            tracing::warn!(file = %file.display(), error = %e, "cannot read server settings, using defaults");
            return ServerSettings::default();
        }
    };
    match serde_json::from_str::<ServerSettings>(&text) {
        Ok(mut s) => {
            if s.view_distance.is_some_and(|v| !valid_view_distance(v)) {
                tracing::warn!(file = %file.display(), "view distance out of range, using the default");
                s.view_distance = None;
            }
            s
        }
        Err(e) => {
            tracing::warn!(file = %file.display(), error = %e, "server settings are not valid, using defaults");
            ServerSettings::default()
        }
    }
}

/// Writes the settings atomically (temp file, flush, rename) so a power cut
/// leaves either the old file or the new one.
pub fn save(server_dir: &Path, settings: &ServerSettings) -> io::Result<()> {
    let file = path(server_dir);
    let tmp = server_dir.join(format!("{FILE_NAME}.tmp"));
    let text = serde_json::to_string_pretty(settings).map_err(io::Error::other)?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &file)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn missing_file_means_defaults() -> TestResult {
        let dir = tempfile::tempdir()?;
        assert_eq!(load(dir.path()), ServerSettings::default());
        Ok(())
    }

    #[test]
    fn saved_settings_are_read_back() -> TestResult {
        let dir = tempfile::tempdir()?;
        let s = ServerSettings {
            view_distance: Some(8),
        };
        save(dir.path(), &s)?;
        assert_eq!(load(dir.path()), s);
        assert!(!dir.path().join("settings.json.tmp").exists());
        save(dir.path(), &ServerSettings::default())?;
        assert_eq!(load(dir.path()).view_distance, None);
        Ok(())
    }

    #[test]
    fn unusable_files_fall_back_to_defaults() -> TestResult {
        let dir = tempfile::tempdir()?;
        for text in [
            "",
            "not json",
            "[]",
            r#"{"view_distance": 99}"#,
            r#"{"view_distance": 1}"#,
            r#"{"view_distance": -3}"#,
            r#"{"view_distance": "far"}"#,
        ] {
            std::fs::write(dir.path().join("settings.json"), text)?;
            assert_eq!(load(dir.path()), ServerSettings::default(), "{text:?}");
        }
        Ok(())
    }

    #[test]
    fn unknown_fields_from_a_newer_release_are_ignored() -> TestResult {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"view_distance": 6, "something_new": true}"#,
        )?;
        assert_eq!(load(dir.path()).view_distance, Some(6));
        Ok(())
    }
}
