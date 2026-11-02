//! `craft-a-server update check|apply`.
//!
//! Design (docs/releasing.md, docs/security-review.md):
//!
//! * Nothing runs unless an administrator types the command: there is no
//!   timer, no daemon-side check, no auto-apply.
//! * Metadata and archives come only from the configured `update_url` over
//!   HTTPS. `SHA256SUMS` and its minisign signature are fetched first and
//!   verified against the keys compiled into this binary; the archive is then
//!   fetched and checked against the signed hash. Nothing is written to disk
//!   before both checks pass.
//! * A release that is not newer than the installed version is never applied
//!   (no downgrades, no reinstall of the same version).
//! * Installing is delegated to the installed `install.sh --upgrade`, which
//!   verifies the same files again with its own copy of the keys, backs up the
//!   database, keeps the previous version for rollback and rolls back by itself
//!   if the new daemon does not come up.

pub mod command;
pub mod fetch;
pub mod keys;
pub mod verify;
pub mod version;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use craft_a_server_core::config::is_https_base_url;
use minisign_verify::PublicKey;

use fetch::{Fetch, FetchError};
use keys::KeyError;
use verify::{Release, VerifyError};
use version::Version;

/// Largest `SHA256SUMS` and signature we will read.
const MAX_SUMS_BYTES: u64 = 64 * 1024;
const MAX_SIG_BYTES: u64 = 4 * 1024;
/// Largest release archive (the binaries are far smaller).
pub const MAX_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;

/// Where `install.sh` lives once installed.
pub const INSTALLED_INSTALLER: &str = "/opt/craft-a-server/current/install.sh";
/// Root-only staging directory for a verified release.
pub const STAGING_DIR: &str = "/var/cache/craft-a-server/update";

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error(transparent)]
    Keys(#[from] KeyError),
    #[error(
        "no update address is configured: set update_url in /etc/craft-a-server/config.toml \
         (this build only knows a placeholder address)"
    )]
    NotConfigured,
    #[error("update_url must be an https:// address")]
    NotHttps,
    #[error("no release builds exist for the CPU architecture {0}")]
    UnsupportedPlatform(String),
    #[error("could not download {what}: {source}")]
    Download {
        what: &'static str,
        source: FetchError,
    },
    #[error("refusing this release: {0}")]
    Verify(#[from] VerifyError),
    #[error("the download does not match the checksum in the signed list; nothing was installed")]
    ArchiveHash,
    #[error(
        "the newest signed release ({offered}) is older than the installed version \
         ({installed}); refusing to downgrade"
    )]
    Downgrade {
        installed: Version,
        offered: Version,
    },
    #[error("could not prepare {path}: {reason}")]
    Staging { path: String, reason: String },
    #[error("the installer is missing at {0}; reinstall with install.sh")]
    InstallerMissing(String),
    #[error("could not run the installer: {0}")]
    InstallerSpawn(String),
    #[error(
        "the installer failed (exit status {0}); the previous version was restored where possible, see its output"
    )]
    InstallerFailed(String),
}

/// Everything `check` and `apply` need, resolved up front.
#[derive(Debug)]
pub struct Settings {
    base_url: String,
    keys: Vec<PublicKey>,
    triple: &'static str,
    current: Version,
}

/// Release target triple for a CPU architecture name (`std::env::consts::ARCH`).
pub fn triple_for(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("x86_64-unknown-linux-gnu"),
        "aarch64" => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

impl Settings {
    /// Validates the inputs. Fails closed: placeholder keys, a placeholder or
    /// non-https address, an unsupported architecture or an unparsable own
    /// version all stop here, before any network access.
    pub fn new(
        update_url: &str,
        keys: Result<Vec<PublicKey>, KeyError>,
        arch: &str,
        current_version: &str,
    ) -> Result<Settings, UpdateError> {
        let keys = keys?;
        if !is_https_base_url(update_url) {
            return Err(UpdateError::NotHttps);
        }
        if update_url.contains("/OWNER/") {
            return Err(UpdateError::NotConfigured);
        }
        let triple =
            triple_for(arch).ok_or_else(|| UpdateError::UnsupportedPlatform(arch.to_owned()))?;
        let current = Version::parse(current_version).ok_or_else(|| {
            UpdateError::UnsupportedPlatform(format!("version {current_version}"))
        })?;
        Ok(Settings {
            base_url: update_url.trim_end_matches('/').to_owned(),
            keys,
            triple,
            current,
        })
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base_url)
    }

    pub fn host(&self) -> &str {
        self.base_url
            .strip_prefix("https://")
            .and_then(|r| r.split('/').next())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    UpToDate,
    Available,
    /// The signed list offers something older than what is installed.
    Older,
}

/// A release list that passed signature verification.
#[derive(Debug)]
pub struct Checked {
    pub current: Version,
    pub release: Release,
    pub status: Status,
    sums: Vec<u8>,
    signature: String,
}

fn download(
    fetch: &dyn Fetch,
    url: &str,
    max: u64,
    what: &'static str,
) -> Result<Vec<u8>, UpdateError> {
    fetch
        .get(url, max)
        .map_err(|source| UpdateError::Download { what, source })
}

/// Downloads and verifies the signed release list and decides what it means.
/// Changes nothing on disk.
pub fn check(fetch: &dyn Fetch, settings: &Settings) -> Result<Checked, UpdateError> {
    let sums = download(
        fetch,
        &settings.url("SHA256SUMS"),
        MAX_SUMS_BYTES,
        "SHA256SUMS",
    )?;
    let signature_bytes = download(
        fetch,
        &settings.url("SHA256SUMS.minisig"),
        MAX_SIG_BYTES,
        "SHA256SUMS.minisig",
    )?;
    let signature =
        String::from_utf8(signature_bytes).map_err(|_| VerifyError::BadSignatureFormat)?;
    verify::verify_signature(&settings.keys, &sums, &signature)?;
    // Only now is the list believed.
    let text = std::str::from_utf8(&sums).map_err(|_| VerifyError::BadSums("not UTF-8"))?;
    let release = verify::pick_release(&verify::parse_sums(text)?, settings.triple)?;
    let status = match release.version.cmp(&settings.current) {
        std::cmp::Ordering::Greater => Status::Available,
        std::cmp::Ordering::Equal => Status::UpToDate,
        std::cmp::Ordering::Less => Status::Older,
    };
    Ok(Checked {
        current: settings.current,
        release,
        status,
        sums,
        signature,
    })
}

/// Runs the installer. A trait so tests never execute anything.
pub trait Runner {
    /// Returns `Ok(None)` for success, `Ok(Some(description))` for a failed
    /// exit status.
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<Option<String>>;
}

/// Runs the program with a scrubbed environment and inherited stdio.
#[derive(Debug, Default)]
pub struct ProcessRunner;

impl Runner for ProcessRunner {
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<Option<String>> {
        let status = std::process::Command::new(program)
            .args(args)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .status()?;
        Ok(if status.success() {
            None
        } else {
            Some(status.to_string())
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    AlreadyCurrent(Version),
    Declined,
    Installed(Version),
}

/// Where `apply` puts things; production uses [`Paths::production`].
#[derive(Debug, Clone)]
pub struct Paths {
    pub staging: PathBuf,
    pub installer: PathBuf,
}

impl Paths {
    pub fn production() -> Paths {
        Paths {
            staging: PathBuf::from(STAGING_DIR),
            installer: PathBuf::from(INSTALLED_INSTALLER),
        }
    }
}

fn staging_error(path: &Path, e: &io::Error) -> UpdateError {
    UpdateError::Staging {
        path: path.display().to_string(),
        reason: e.to_string(),
    }
}

/// Creates an empty, owner-only directory (replacing a leftover one).
fn fresh_private_dir(path: &Path) -> Result<(), UpdateError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(staging_error(path, &e)),
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| staging_error(path, &e))
}

/// Verifies, downloads, re-verifies and hands the release to the installer.
///
/// `confirm` is asked once the release is verified and known to be newer, and
/// before anything is written; returning `false` ends the run untouched.
pub fn apply(
    fetch: &dyn Fetch,
    settings: &Settings,
    paths: &Paths,
    runner: &dyn Runner,
    confirm: &mut dyn FnMut(&Checked) -> bool,
) -> Result<Applied, UpdateError> {
    let checked = check(fetch, settings)?;
    match checked.status {
        Status::UpToDate => return Ok(Applied::AlreadyCurrent(checked.current)),
        Status::Older => {
            return Err(UpdateError::Downgrade {
                installed: checked.current,
                offered: checked.release.version,
            });
        }
        Status::Available => {}
    }
    if !paths.installer.is_file() {
        return Err(UpdateError::InstallerMissing(
            paths.installer.display().to_string(),
        ));
    }
    if !confirm(&checked) {
        return Ok(Applied::Declined);
    }
    let archive = download(
        fetch,
        &settings.url(&checked.release.name),
        MAX_ARCHIVE_BYTES,
        "the release archive",
    )?;
    if !verify::hash_matches(&archive, &checked.release.sha256) {
        return Err(UpdateError::ArchiveHash);
    }

    fresh_private_dir(&paths.staging)?;
    let result = stage_and_install(&checked, &archive, paths, runner);
    // The staged files are re-downloadable; never leave them behind.
    let _ = std::fs::remove_dir_all(&paths.staging);
    result.map(|()| Applied::Installed(checked.release.version))
}

fn stage_and_install(
    checked: &Checked,
    archive: &[u8],
    paths: &Paths,
    runner: &dyn Runner,
) -> Result<(), UpdateError> {
    let write = |name: &str, bytes: &[u8]| {
        let path = paths.staging.join(name);
        std::fs::write(&path, bytes).map_err(|e| staging_error(&path, &e))
    };
    write("SHA256SUMS", &checked.sums)?;
    write("SHA256SUMS.minisig", checked.signature.as_bytes())?;
    write(&checked.release.name, archive)?;
    let args = [
        OsString::from("--upgrade"),
        OsString::from("--release-dir"),
        paths.staging.clone().into_os_string(),
        OsString::from("--yes"),
    ];
    match runner.run(&paths.installer, &args) {
        Ok(None) => Ok(()),
        Ok(Some(status)) => Err(UpdateError::InstallerFailed(status)),
        Err(e) => Err(UpdateError::InstallerSpawn(e.to_string())),
    }
}

#[cfg(test)]
mod tests;
