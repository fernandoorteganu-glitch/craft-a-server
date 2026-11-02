//! Terminal side of `update check|apply`: config, output, confirmation.

use std::io::{self, BufRead, Write};
use std::path::Path;

use craft_a_server_core::config::{Config, DEFAULT_UPDATE_URL};

use super::fetch::UreqFetch;
use super::{Applied, Checked, Paths, ProcessRunner, Settings, Status, UpdateError, keys};
use crate::args::{UpdateCmd, is_yes};
use crate::render::sanitize;

const EXIT_ERROR: u8 = 1;
const OWN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Loads the update address. A missing config file means defaults; a broken
/// one is an error (it is the administrator's file, not a guess to be made).
fn update_url(config_path: &Path) -> Result<String, String> {
    let config = match std::fs::read_to_string(config_path) {
        Ok(text) => {
            Config::from_toml(&text).map_err(|e| format!("{}: {e}", config_path.display()))?
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("{}: {e}", config_path.display())),
    };
    Ok(config
        .update_url
        .unwrap_or_else(|| DEFAULT_UPDATE_URL.to_owned()))
}

fn settings(config_path: &Path) -> Result<Settings, String> {
    let url = update_url(config_path)?;
    Settings::new(
        &url,
        keys::embedded_keys(),
        std::env::consts::ARCH,
        OWN_VERSION,
    )
    .map_err(|e| e.to_string())
}

pub fn run(config_path: &Path, json: bool, assume_yes: bool, action: &UpdateCmd) -> u8 {
    let settings = match settings(config_path) {
        Ok(s) => s,
        Err(message) => {
            eprintln!("error: {}", sanitize(&message));
            return EXIT_ERROR;
        }
    };
    let fetch = UreqFetch::new();
    match action {
        UpdateCmd::Check => {
            eprintln!("Contacting {} ...", sanitize(settings.host()));
            match super::check(&fetch, &settings) {
                Ok(checked) => {
                    print_check(&checked, json);
                    0
                }
                Err(e) => fail(&e),
            }
        }
        UpdateCmd::Apply => {
            eprintln!("Contacting {} ...", sanitize(settings.host()));
            let mut confirm = |checked: &Checked| {
                if assume_yes {
                    return true;
                }
                eprintln!(
                    "Version {} is available (installed: {}). Installing it briefly stops the \
                     craft-a-server daemon (game servers keep running), backs up the database \
                     and keeps the current version for rollback.",
                    checked.release.version, checked.current
                );
                eprint!("Continue? [y/N] ");
                let _ = io::stderr().flush();
                let mut answer = String::new();
                io::stdin().lock().read_line(&mut answer).is_ok() && is_yes(&answer)
            };
            match super::apply(
                &fetch,
                &settings,
                &Paths::production(),
                &ProcessRunner,
                &mut confirm,
            ) {
                Ok(Applied::Installed(v)) => {
                    println!("Updated to {v}.");
                    0
                }
                Ok(Applied::AlreadyCurrent(v)) => {
                    println!("Already up to date ({v}).");
                    0
                }
                Ok(Applied::Declined) => {
                    eprintln!("Cancelled.");
                    EXIT_ERROR
                }
                Err(e) => fail(&e),
            }
        }
    }
}

fn fail(e: &UpdateError) -> u8 {
    eprintln!("error: {}", sanitize(&e.to_string()));
    EXIT_ERROR
}

fn print_check(checked: &Checked, json: bool) {
    let latest = checked.release.version;
    if json {
        let status = match checked.status {
            Status::UpToDate => "up_to_date",
            Status::Available => "available",
            Status::Older => "older",
        };
        println!(
            "{}",
            serde_json::json!({
                "installed": checked.current.to_string(),
                "latest": latest.to_string(),
                "status": status,
            })
        );
        return;
    }
    println!("Installed:             {}", checked.current);
    println!("Newest signed release: {latest} (signature and list verified)");
    match checked.status {
        Status::Available => {
            println!("An update is available. Install it with: sudo craft-a-server update apply")
        }
        Status::UpToDate => println!("You are up to date."),
        Status::Older => println!(
            "The newest signed release is older than the installed version; it will not be offered."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_uses_the_default_address() {
        let dir = tempfile::tempdir().map(|d| d.path().join("absent.toml"));
        let url = dir.map_err(|e| e.to_string()).and_then(|p| update_url(&p));
        assert_eq!(url.as_deref(), Ok(DEFAULT_UPDATE_URL));
    }

    #[test]
    fn config_address_wins_and_broken_config_is_an_error() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "update_url = \"https://example.org/rel\"\n")?;
        assert_eq!(update_url(&path).as_deref(), Ok("https://example.org/rel"));
        std::fs::write(&path, "update_url = \"http://example.org/rel\"\n")?;
        assert!(update_url(&path).is_err());
        std::fs::write(&path, "no_such_key = 1\n")?;
        assert!(update_url(&path).is_err());
        Ok(())
    }

    #[test]
    fn the_shipped_build_has_a_key_and_an_address() {
        // No config file: the built-in https address and the embedded release
        // key are used, so the settings validate (on a supported architecture).
        let dir = tempfile::tempdir().map(|d| d.path().join("absent.toml"));
        let result = dir.map_err(|e| e.to_string()).and_then(|p| settings(&p));
        assert!(result.is_ok(), "{result:?}");
    }
}
