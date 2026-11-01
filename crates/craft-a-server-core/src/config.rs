//! Daemon configuration, loaded from `/etc/craft-a-server/config.toml`.
//!
//! Every field has a safe default so a missing file is a valid setup; unknown
//! keys are rejected so a typo cannot silently disable a safety setting.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_MC_PORT: u16 = 25565;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("invalid configuration file: {0}")]
    Parse(String),
    #[error("invalid setting: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Root for servers, backups and the database.
    pub data_dir: PathBuf,
    /// Unix socket the CLI uses to talk to the daemon.
    pub socket_path: PathBuf,
    /// Podman binary to call.
    pub podman_bin: String,
    /// Server image, ideally pinned by digest in releases.
    pub image: String,
    /// Host port the Minecraft server is published on.
    pub mc_port: u16,
    /// CPUs a server container may use (0 = all).
    pub cpus: u32,
    /// Process limit inside the container.
    pub pids_limit: u32,
    /// RAM kept for the OS, the daemon and the tunnel agent (MiB).
    pub memory_reserve_mib: u64,
    /// Optional cap on the game's memory (MiB).
    pub memory_max_mib: Option<u64>,
    /// How long a server may take to answer its first status ping.
    pub start_timeout_secs: u64,
    /// How long `stop` may take before the container is forced down.
    pub stop_timeout_secs: u64,
    /// Consecutive failed starts before a server stays `failed`.
    pub max_consecutive_failures: u32,
    /// Block backups and installs when free disk drops below this percentage.
    pub min_free_disk_percent: u8,
    /// Hour (UTC, 0-23) the daily backup runs.
    pub daily_backup_hour_utc: u8,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::from("/var/lib/craft-a-server"),
            socket_path: PathBuf::from("/run/craft-a-server/ctl.sock"),
            podman_bin: "podman".into(),
            image: "docker.io/itzg/minecraft-server:latest".into(),
            mc_port: DEFAULT_MC_PORT,
            cpus: 0,
            pids_limit: 512,
            memory_reserve_mib: crate::memory::DEFAULT_RESERVE_MIB,
            memory_max_mib: None,
            start_timeout_secs: 300,
            stop_timeout_secs: 60,
            max_consecutive_failures: 3,
            min_free_disk_percent: 15,
            daily_backup_hour_utc: 4,
        }
    }
}

impl Config {
    /// Parses and validates a TOML document.
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: &str| Err(ConfigError::Invalid(m.into()));
        if !self.data_dir.has_root() {
            return bad("data_dir must be an absolute path");
        }
        if !self.socket_path.has_root() {
            return bad("socket_path must be an absolute path");
        }
        if self.mc_port < 1024 {
            return bad("mc_port must be 1024 or higher (the server runs without privileges)");
        }
        if self.pids_limit < 64 {
            return bad("pids_limit must be at least 64");
        }
        if self.start_timeout_secs == 0 || self.stop_timeout_secs == 0 {
            return bad("timeouts must be greater than zero");
        }
        if self.max_consecutive_failures == 0 {
            return bad("max_consecutive_failures must be at least 1");
        }
        if !(1..=90).contains(&self.min_free_disk_percent) {
            return bad("min_free_disk_percent must be between 1 and 90");
        }
        if self.daily_backup_hour_utc > 23 {
            return bad("daily_backup_hour_utc must be between 0 and 23");
        }
        if self.image.is_empty() || self.image.chars().any(char::is_whitespace) {
            return bad("image must be a non-empty reference without spaces");
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("craft-a-server.db")
    }

    pub fn servers_dir(&self) -> PathBuf {
        self.data_dir.join("servers")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        assert_eq!(Config::default().validate(), Ok(()));
    }

    #[test]
    fn empty_document_gives_defaults() {
        assert_eq!(Config::from_toml(""), Ok(Config::default()));
    }

    #[test]
    fn overrides_apply() -> Result<(), ConfigError> {
        let c = Config::from_toml("mc_port = 25570\nmemory_max_mib = 4096\n")?;
        assert_eq!(c.mc_port, 25570);
        assert_eq!(c.memory_max_mib, Some(4096));
        Ok(())
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(matches!(
            Config::from_toml("mc_prot = 1\n"),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn unsafe_values_are_rejected() {
        for doc in [
            "mc_port = 80",
            "data_dir = \"relative/path\"",
            "pids_limit = 1",
            "stop_timeout_secs = 0",
            "min_free_disk_percent = 0",
            "daily_backup_hour_utc = 24",
            "max_consecutive_failures = 0",
            "image = \"a b\"",
        ] {
            assert!(
                matches!(Config::from_toml(doc), Err(ConfigError::Invalid(_))),
                "{doc} should be invalid"
            );
        }
    }

    #[test]
    fn derived_paths() {
        let c = Config::default();
        assert!(c.db_path().ends_with("craft-a-server.db"));
        assert!(c.servers_dir().ends_with("servers"));
        assert!(c.backups_dir().ends_with("backups"));
    }
}
