//! Installing the pinned agent: download, verify, then (and only then) make
//! executable. Also the files the tunnel keeps on disk: a private directory
//! and the secret.
//!
//! Order of events for one asset:
//!
//! 1. download from the pinned release URL (redirects only between GitHub's
//!    own hosts, size capped) into memory;
//! 2. check length and SHA-256 against the compiled-in table;
//! 3. write to a temporary file created `0600` in the private directory;
//! 4. `fsync`, change the mode to `0755`, rename into place.
//!
//! Nothing is written, let alone made executable, before step 2 passes. The
//! hash is checked again from disk right before every launch
//! ([`verify_installed`]).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::fetch::{DownloadError, HttpFetch, download};
use super::release::{
    MAX_ASSET_BYTES, PLACEHOLDER_SHA256, PinnedAsset, Platform, Release, is_real_sha256,
};

/// The two executables the tunnel runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    /// `playitd`, the background agent.
    pub daemon: PathBuf,
    /// `playit-cli`, used once to link the agent to an account.
    pub cli: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("online play is not available on this kind of computer")]
    Unsupported,
    #[error("this build has no verified checksum for {0}, so it will not be downloaded")]
    Unverifiable(String),
    #[error("{0} is not part of the pinned release")]
    NotPinned(String),
    #[error("could not download the playit agent: {0}")]
    Download(#[from] DownloadError),
    #[error("the downloaded {0} does not match the pinned checksum and was discarded")]
    HashMismatch(String),
    #[error("the downloaded {0} has an unexpected size and was discarded")]
    SizeMismatch(String),
    #[error("the installed {0} does not match the pinned checksum and will not be run")]
    Tampered(String),
    #[error("could not write the playit agent to disk: {0}")]
    Io(#[from] io::Error),
}

impl InstallError {
    /// A permanent error will not fix itself by retrying.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            InstallError::Unsupported | InstallError::Unverifiable(_) | InstallError::NotPinned(_)
        )
    }

    /// Short text safe to show (no paths).
    pub fn user_text(&self) -> String {
        match self {
            InstallError::Io(_) => "could not write the playit agent to disk".to_owned(),
            other => other.to_string(),
        }
    }
}

/// Lower-case hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Creates `path` (and parents) with mode `0700` and tightens it if it
/// already exists with wider permissions.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

fn check_asset(asset: &PinnedAsset) -> Result<(), InstallError> {
    if asset.sha256 == PLACEHOLDER_SHA256 || !is_real_sha256(&asset.sha256) {
        return Err(InstallError::Unverifiable(asset.name.clone()));
    }
    if asset.size == 0 || asset.size > MAX_ASSET_BYTES {
        return Err(InstallError::NotPinned(asset.name.clone()));
    }
    Ok(())
}

fn write_executable(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let final_path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // Only now, with the content verified and on disk, it becomes runnable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &final_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    #[cfg(unix)]
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(final_path)
}

/// Reads the installed file and checks it against the pin. Used before every
/// launch.
pub fn verify_installed(path: &Path, asset: &PinnedAsset) -> Result<(), InstallError> {
    check_asset(asset)?;
    let tampered = || InstallError::Tampered(asset.name.clone());
    let meta = std::fs::symlink_metadata(path).map_err(|_| tampered())?;
    if !meta.is_file() || meta.len() != asset.size {
        return Err(tampered());
    }
    let bytes = std::fs::read(path).map_err(|_| tampered())?;
    if sha256_hex(&bytes) != asset.sha256 {
        return Err(tampered());
    }
    Ok(())
}

fn install_one(
    fetch: &dyn HttpFetch,
    release: &Release,
    asset_name: &str,
    dir: &Path,
) -> Result<PathBuf, InstallError> {
    let asset = release
        .asset(asset_name)
        .ok_or_else(|| InstallError::NotPinned(asset_name.to_owned()))?;
    check_asset(asset)?;
    let path = dir.join(&asset.name);
    if verify_installed(&path, asset).is_ok() {
        return Ok(path);
    }
    // Missing, truncated or modified: replace it from the pinned source.
    let _ = std::fs::remove_file(&path);
    let bytes = download(
        fetch,
        &release.url_of(asset),
        asset.size.min(MAX_ASSET_BYTES),
    )?;
    if bytes.len() as u64 != asset.size {
        return Err(InstallError::SizeMismatch(asset.name.clone()));
    }
    if sha256_hex(&bytes) != asset.sha256 {
        return Err(InstallError::HashMismatch(asset.name.clone()));
    }
    Ok(write_executable(dir, &asset.name, &bytes)?)
}

/// Makes sure both executables of `release` for `platform` are installed in
/// `bin_dir` and verified. Blocking (network and disk): call it from
/// `spawn_blocking`.
pub fn ensure_installed(
    fetch: &dyn HttpFetch,
    release: &Release,
    platform: Option<Platform>,
    bin_dir: &Path,
) -> Result<AgentPaths, InstallError> {
    let platform = platform.ok_or(InstallError::Unsupported)?;
    // Refuse before touching the network or the disk if either hash is unusable.
    for name in [platform.daemon_asset(), platform.cli_asset()] {
        let asset = release
            .asset(name)
            .ok_or_else(|| InstallError::NotPinned(name.to_owned()))?;
        check_asset(asset)?;
    }
    let dir = bin_dir.join(&release.version);
    ensure_private_dir(bin_dir)?;
    ensure_private_dir(&dir)?;
    Ok(AgentPaths {
        daemon: install_one(fetch, release, platform.daemon_asset(), &dir)?,
        cli: install_one(fetch, release, platform.cli_asset(), &dir)?,
    })
}

/// Re-checks both installed files against the pin.
pub fn verify_paths(
    release: &Release,
    platform: Option<Platform>,
    paths: &AgentPaths,
) -> Result<(), InstallError> {
    let platform = platform.ok_or(InstallError::Unsupported)?;
    for (name, path) in [
        (platform.daemon_asset(), &paths.daemon),
        (platform.cli_asset(), &paths.cli),
    ] {
        let asset = release
            .asset(name)
            .ok_or_else(|| InstallError::NotPinned(name.to_owned()))?;
        verify_installed(path, asset)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::fetch::fake::{Canned, FakeFetch};
    use super::super::release::RELEASE_BASE;
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    pub(crate) const DAEMON_BYTES: &[u8] = b"#!/bin/sh\necho daemon\n";
    pub(crate) const CLI_BYTES: &[u8] = b"#!/bin/sh\necho cli\n";

    /// A release whose two linux-amd64 assets are the byte strings above.
    pub(crate) fn test_release() -> Release {
        let mk = |name: &str, bytes: &[u8]| PinnedAsset {
            name: name.to_owned(),
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
        };
        Release {
            version: "9.9.9".into(),
            base_url: RELEASE_BASE.to_owned(),
            assets: vec![
                mk("playit-linux-amd64", DAEMON_BYTES),
                mk("playit-cli-linux-amd64", CLI_BYTES),
            ],
        }
    }

    fn serve_good(fetch: &FakeFetch, release: &Release) {
        for (name, bytes) in [
            ("playit-linux-amd64", DAEMON_BYTES),
            ("playit-cli-linux-amd64", CLI_BYTES),
        ] {
            if let Some(asset) = release.asset(name) {
                fetch.route(&release.url_of(asset), Canned::Ok(bytes.to_vec()));
            }
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn installs_verified_files_and_reuses_them() -> TestResult {
        let dir = tempfile::tempdir()?;
        let release = test_release();
        let fetch = FakeFetch::default();
        serve_good(&fetch, &release);
        let bin = dir.path().join("bin");
        let paths = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), &bin)?;
        assert_eq!(std::fs::read(&paths.daemon)?, DAEMON_BYTES);
        assert_eq!(std::fs::read(&paths.cli)?, CLI_BYTES);
        assert_eq!(fetch.requests().len(), 2);
        verify_paths(&release, Some(Platform::LinuxAmd64), &paths)?;
        // A second call finds both files intact and downloads nothing.
        let again = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), &bin)?;
        assert_eq!(again, paths);
        assert_eq!(fetch.requests().len(), 2);
        // No temporary files are left behind.
        let version_dir = bin.join("9.9.9");
        assert_eq!(
            entries(&version_dir),
            ["playit-cli-linux-amd64", "playit-linux-amd64"]
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_private_dir_and_executable_files() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let release = test_release();
        let fetch = FakeFetch::default();
        serve_good(&fetch, &release);
        let bin = dir.path().join("bin");
        let paths = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), &bin)?;
        let mode = |p: &Path| std::fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode(&bin)?, 0o700);
        assert_eq!(mode(&bin.join("9.9.9"))?, 0o700);
        assert_eq!(mode(&paths.daemon)?, 0o755);
        assert_eq!(mode(&paths.cli)?, 0o755);
        Ok(())
    }

    #[test]
    fn a_hash_mismatch_is_discarded_and_nothing_is_written() -> TestResult {
        let dir = tempfile::tempdir()?;
        let release = test_release();
        let fetch = FakeFetch::default();
        serve_good(&fetch, &release);
        // The daemon asset is swapped for different bytes of the same length.
        let mut evil = DAEMON_BYTES.to_vec();
        if let Some(b) = evil.get_mut(12) {
            *b ^= 1;
        }
        let url = release
            .asset("playit-linux-amd64")
            .map(|a| release.url_of(a))
            .ok_or("asset")?;
        fetch.route(&url, Canned::Ok(evil));
        let bin = dir.path().join("bin");
        let err = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), &bin)
            .err()
            .ok_or("must fail")?;
        assert!(matches!(err, InstallError::HashMismatch(_)), "{err}");
        assert!(!err.is_permanent());
        assert!(entries(&bin.join("9.9.9")).is_empty());
        Ok(())
    }

    #[test]
    fn a_wrong_size_is_discarded() -> TestResult {
        let dir = tempfile::tempdir()?;
        let release = test_release();
        let fetch = FakeFetch::default();
        serve_good(&fetch, &release);
        let url = release
            .asset("playit-linux-amd64")
            .map(|a| release.url_of(a))
            .ok_or("asset")?;
        let mut short = DAEMON_BYTES.to_vec();
        short.pop();
        fetch.route(&url, Canned::Ok(short));
        let err = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), dir.path())
            .err()
            .ok_or("must fail")?;
        assert!(matches!(err, InstallError::SizeMismatch(_)), "{err}");
        Ok(())
    }

    #[test]
    fn placeholder_hashes_fail_closed_before_any_request() -> TestResult {
        let dir = tempfile::tempdir()?;
        let mut release = test_release();
        for a in &mut release.assets {
            if a.name == "playit-cli-linux-amd64" {
                a.sha256 = PLACEHOLDER_SHA256.to_owned();
            }
        }
        let fetch = FakeFetch::default();
        serve_good(&fetch, &release);
        let err = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), dir.path())
            .err()
            .ok_or("must fail")?;
        assert!(matches!(err, InstallError::Unverifiable(_)), "{err}");
        assert!(err.is_permanent());
        assert!(fetch.requests().is_empty(), "nothing may be downloaded");
        assert!(entries(dir.path()).is_empty(), "nothing may be written");
        Ok(())
    }

    #[test]
    fn unsupported_platforms_and_unpinned_assets_are_permanent_errors() -> TestResult {
        let dir = tempfile::tempdir()?;
        let release = test_release();
        let fetch = FakeFetch::default();
        let err = ensure_installed(&fetch, &release, None, dir.path())
            .err()
            .ok_or("must fail")?;
        assert!(matches!(err, InstallError::Unsupported));
        assert!(err.is_permanent());
        // aarch64 is not in the test release.
        let err = ensure_installed(&fetch, &release, Some(Platform::LinuxAarch64), dir.path())
            .err()
            .ok_or("must fail")?;
        assert!(matches!(err, InstallError::NotPinned(_)));
        Ok(())
    }

    #[test]
    fn a_modified_installed_file_is_not_run_and_is_replaced_on_reinstall() -> TestResult {
        let dir = tempfile::tempdir()?;
        let release = test_release();
        let fetch = FakeFetch::default();
        serve_good(&fetch, &release);
        let bin = dir.path().join("bin");
        let paths = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), &bin)?;
        std::fs::write(&paths.daemon, b"#!/bin/sh\nrm -rf /\n")?;
        let err = verify_paths(&release, Some(Platform::LinuxAmd64), &paths)
            .err()
            .ok_or("must refuse")?;
        assert!(matches!(err, InstallError::Tampered(_)), "{err}");
        // Re-installing downloads the pinned bytes again.
        let fixed = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), &bin)?;
        assert_eq!(std::fs::read(&fixed.daemon)?, DAEMON_BYTES);
        verify_paths(&release, Some(Platform::LinuxAmd64), &fixed)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_place_of_the_binary_is_refused() -> TestResult {
        let dir = tempfile::tempdir()?;
        let release = test_release();
        let fetch = FakeFetch::default();
        serve_good(&fetch, &release);
        let bin = dir.path().join("bin");
        let paths = ensure_installed(&fetch, &release, Some(Platform::LinuxAmd64), &bin)?;
        let real = dir.path().join("real");
        std::fs::write(&real, DAEMON_BYTES)?;
        std::fs::remove_file(&paths.daemon)?;
        std::os::unix::fs::symlink(&real, &paths.daemon)?;
        assert!(verify_paths(&release, Some(Platform::LinuxAmd64), &paths).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_is_tightened() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("t");
        std::fs::create_dir(&p)?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
        ensure_private_dir(&p)?;
        assert_eq!(std::fs::metadata(&p)?.permissions().mode() & 0o777, 0o700);
        Ok(())
    }

    #[test]
    fn real_pinned_release_passes_the_pre_checks() -> TestResult {
        // No network: the pinned hashes must at least be usable (not placeholders).
        let release = Release::pinned();
        for platform in [Platform::LinuxAmd64, Platform::LinuxAarch64] {
            for name in [platform.daemon_asset(), platform.cli_asset()] {
                let asset = release.asset(name).ok_or("missing")?;
                check_asset(asset)?;
            }
        }
        Ok(())
    }
}
