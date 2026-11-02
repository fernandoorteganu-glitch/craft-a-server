//! Daemon configuration, loaded from `/etc/craft-a-server/config.toml`.
//!
//! Every field has a safe default so a missing file is a valid setup; unknown
//! keys are rejected so a typo cannot silently disable a safety setting.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_MC_PORT: u16 = 25565;
pub const DEFAULT_PANEL_PORT: u16 = 8080;
pub const DEFAULT_PANEL_HOSTNAME: &str = "craft-a-server.local";

/// True for a plain DNS hostname: dot-separated labels of ASCII letters,
/// digits and inner hyphens (no port, no scheme, no trailing dot).
pub fn is_plain_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

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
    /// TCP port of the local web panel.
    pub panel_port: u16,
    /// Address the panel listens on (`0.0.0.0` for the LAN, `::` for dual stack).
    pub panel_bind: String,
    /// Name the panel answers to besides `localhost` and private IP literals
    /// (the Host-header allow-list; also used in the URL shown by `pair`).
    pub panel_hostname: String,
    /// How long a pairing code stays valid.
    pub pairing_code_ttl_secs: u64,
    /// How long a paired phone stays signed in without being used.
    pub session_ttl_secs: u64,
    /// Contact (email or URL) sent in the `User-Agent` to Modrinth, as their
    /// API rules ask for an identifiable client. Optional.
    pub modrinth_contact: Option<String>,
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
            panel_port: DEFAULT_PANEL_PORT,
            panel_bind: "0.0.0.0".into(),
            panel_hostname: DEFAULT_PANEL_HOSTNAME.into(),
            pairing_code_ttl_secs: 600,
            session_ttl_secs: 30 * 24 * 3600,
            modrinth_contact: None,
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
        if self.panel_port < 1024 {
            return bad("panel_port must be 1024 or higher (the panel runs without privileges)");
        }
        if self.panel_port == self.mc_port {
            return bad("panel_port and mc_port must differ");
        }
        if self.panel_bind.parse::<std::net::IpAddr>().is_err() {
            return bad("panel_bind must be an IP address such as 0.0.0.0 or ::");
        }
        if !is_plain_hostname(&self.panel_hostname) {
            return bad("panel_hostname must be a plain hostname such as craft-a-server.local");
        }
        if !(60..=86_400).contains(&self.pairing_code_ttl_secs) {
            return bad("pairing_code_ttl_secs must be between 60 and 86400");
        }
        if !(3_600..=365 * 24 * 3600).contains(&self.session_ttl_secs) {
            return bad("session_ttl_secs must be between 3600 and 31536000");
        }
        if let Some(contact) = &self.modrinth_contact
            && (contact.len() > 100 || !contact.bytes().all(|b| (0x20..0x7f).contains(&b)))
        {
            return bad("modrinth_contact must be at most 100 printable ASCII characters");
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

    /// Private directory of the optional playit.gg tunnel (agent binaries,
    /// its secret, its socket).
    pub fn tunnel_dir(&self) -> PathBuf {
        self.data_dir.join("tunnel")
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
            "panel_port = 80",
            "panel_port = 25565",
            "panel_bind = \"localhost\"",
            "panel_bind = \"\"",
            "panel_hostname = \"evil.com:80\"",
            "panel_hostname = \"a@b\"",
            "panel_hostname = \"host.\"",
            "panel_hostname = \"-x.local\"",
            "panel_hostname = \"\"",
            "pairing_code_ttl_secs = 5",
            "pairing_code_ttl_secs = 100000",
            "session_ttl_secs = 60",
            "modrinth_contact = \"a\\nb\"",
            "modrinth_contact = \"caf\u{e9}\"",
        ] {
            assert!(
                matches!(Config::from_toml(doc), Err(ConfigError::Invalid(_))),
                "{doc} should be invalid"
            );
        }
    }

    #[test]
    fn panel_defaults_and_overrides() -> Result<(), ConfigError> {
        let d = Config::default();
        assert_eq!(d.panel_port, 8080);
        assert_eq!(d.panel_bind, "0.0.0.0");
        assert_eq!(d.panel_hostname, "craft-a-server.local");
        assert_eq!(d.pairing_code_ttl_secs, 600);
        assert_eq!(d.session_ttl_secs, 2_592_000);
        let c = Config::from_toml(
            "panel_port = 18080
panel_bind = \"::\"
panel_hostname = \"mc.home\"
             pairing_code_ttl_secs = 300
session_ttl_secs = 86400
",
        )?;
        assert_eq!((c.panel_port, c.panel_bind.as_str()), (18080, "::"));
        assert_eq!(c.panel_hostname, "mc.home");
        Ok(())
    }

    #[test]
    fn modrinth_contact_is_optional_and_validated() -> Result<(), ConfigError> {
        assert_eq!(Config::default().modrinth_contact, None);
        let c = Config::from_toml("modrinth_contact = \"me@example.com\"\n")?;
        assert_eq!(c.modrinth_contact.as_deref(), Some("me@example.com"));
        assert!(Config::from_toml(&format!("modrinth_contact = \"{}\"", "a".repeat(101))).is_err());
        Ok(())
    }

    #[test]
    fn hostname_validation() {
        for ok in ["craft-a-server.local", "a", "mc.home", "X1.example"] {
            assert!(is_plain_hostname(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "a..b",
            "a.",
            "-a",
            "a-",
            "a b",
            "a:80",
            "a/b",
            "a@b",
            "é.local",
            &"a".repeat(64),
            &format!(
                "{}.{}.{}.{}.b",
                "a".repeat(63),
                "a".repeat(63),
                "a".repeat(63),
                "a".repeat(63)
            ),
        ] {
            assert!(!is_plain_hostname(bad), "{bad}");
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
