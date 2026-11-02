//! The pinned playit agent release and the rules for where it may come from.
//!
//! playit publishes no signatures for its Linux binaries, so the trust anchor
//! is this table: the SHA-256 (and size) of every asset we are willing to run,
//! compiled into the daemon. A downloaded file that does not match is deleted
//! and never executed.
//!
//! # Updating the pin (maintainer)
//!
//! 1. Pick the release; read `digest` of each asset from
//!    `https://api.github.com/repos/playit-cloud/playit-agent/releases/tags/vX.Y.Z`.
//! 2. Download the assets yourself over HTTPS and run `sha256sum`; the two
//!    must agree (this guards against a bad API response, not a bad release).
//! 3. Put version, sizes and hashes in `PINNED_ASSETS` and [`AGENT_VERSION`].
//! 4. Re-run the spike checks in `docs/spikes/playit-agent.md` (CLI output
//!    formats, rundata shape) before shipping: the 1.x line changes quickly.
//!
//! While a platform's entry holds [`PLACEHOLDER_SHA256`] it can never match,
//! so installing fails closed.

/// The agent version this build installs.
pub const AGENT_VERSION: &str = "1.2.0";

/// Where the pinned assets live. Nothing else is ever downloaded.
pub const RELEASE_BASE: &str =
    "https://github.com/playit-cloud/playit-agent/releases/download/v1.2.0/";

/// A hash value that makes the install fail closed (used while a platform's
/// real hash has not been filled in).
pub const PLACEHOLDER_SHA256: &str = "PLACEHOLDER-NOT-VERIFIED-see-docs-tunnel-md";

/// Largest asset we accept, whatever the table says.
pub const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;

/// Hosts a release download may be redirected to. GitHub serves release
/// assets from these; anything else is refused.
pub const REDIRECT_HOSTS: &[&str] = &[
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

/// Most redirects followed for one download.
pub const MAX_REDIRECTS: usize = 3;

const RELEASE_HOST: &str = "github.com";
const RELEASE_PATH_PREFIX: &str = "/playit-cloud/playit-agent/releases/download/";

/// A CPU/OS combination the pinned release ships binaries for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    LinuxAmd64,
    LinuxAarch64,
}

impl Platform {
    /// The platform this build runs on, if the release supports it.
    pub fn detect() -> Option<Platform> {
        Self::from_parts(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn from_parts(os: &str, arch: &str) -> Option<Platform> {
        match (os, arch) {
            ("linux", "x86_64") => Some(Platform::LinuxAmd64),
            ("linux", "aarch64") => Some(Platform::LinuxAarch64),
            _ => None,
        }
    }

    /// File name of the daemon (`playitd`) asset.
    pub fn daemon_asset(self) -> &'static str {
        match self {
            Platform::LinuxAmd64 => "playit-linux-amd64",
            Platform::LinuxAarch64 => "playit-linux-aarch64",
        }
    }

    /// File name of the command-line frontend asset (claims the agent).
    pub fn cli_asset(self) -> &'static str {
        match self {
            Platform::LinuxAmd64 => "playit-cli-linux-amd64",
            Platform::LinuxAarch64 => "playit-cli-linux-aarch64",
        }
    }
}

/// One asset we are willing to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedAsset {
    pub name: String,
    /// Lower-case hex SHA-256, or [`PLACEHOLDER_SHA256`].
    pub sha256: String,
    /// Exact size in bytes.
    pub size: u64,
}

/// The table compiled into the daemon: values checked against the GitHub
/// release API and against a local download of each file (2026-11-02).
const PINNED_ASSETS: &[(&str, &str, u64)] = &[
    (
        "playit-linux-amd64",
        "602dea3288d5bad4c3addba70ecf6535b9ae085b8d4f2fc2fe9cde913bda78d0",
        6_230_048,
    ),
    (
        "playit-linux-aarch64",
        "3a6d69c348803c7041c49312345c18b1e9cb7dd7fc2adfbf2217e50d0d7c9a34",
        6_533_976,
    ),
    (
        "playit-cli-linux-amd64",
        "a10cd77866bdd81837848ad08b6629012637f2662e7c0b7d9310bcdb619feebd",
        5_618_296,
    ),
    (
        "playit-cli-linux-aarch64",
        "d4cc2cd2e6f4bc9614a2b764eba60a5e0656d71ff8a2720a061e2333daef8940",
        5_887_200,
    ),
];

/// A release: where to download from and what every asset must hash to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub base_url: String,
    pub assets: Vec<PinnedAsset>,
}

impl Release {
    /// The release compiled into this build.
    pub fn pinned() -> Release {
        Release {
            version: AGENT_VERSION.to_owned(),
            base_url: RELEASE_BASE.to_owned(),
            assets: PINNED_ASSETS
                .iter()
                .map(|(name, sha256, size)| PinnedAsset {
                    name: (*name).to_owned(),
                    sha256: (*sha256).to_owned(),
                    size: *size,
                })
                .collect(),
        }
    }

    pub fn asset(&self, name: &str) -> Option<&PinnedAsset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// Download URL of an asset: the pinned base plus the file name.
    pub fn url_of(&self, asset: &PinnedAsset) -> String {
        format!("{}{}", self.base_url, asset.name)
    }
}

/// True for a 64-character lower-case hex string.
pub fn is_real_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn plain_ascii(url: &str) -> bool {
    url.len() <= 2048
        && url.bytes().all(|b| (0x21..0x7f).contains(&b))
        && !url.contains(['\\', '@', '#'])
}

fn split_https(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    Some(rest.split_once('/').unwrap_or((rest, "")))
}

fn has_dot_segment(path: &str) -> bool {
    path.split('/').any(|seg| seg == ".." || seg == ".")
}

/// The first request of a download: `https://github.com/playit-cloud/
/// playit-agent/releases/download/...`, no query, no port, no credentials.
pub fn release_url_allowed(url: &str) -> bool {
    let Some((host, path)) = split_https(url) else {
        return false;
    };
    let full_path = format!("/{path}");
    plain_ascii(url)
        && host == RELEASE_HOST
        && full_path.starts_with(RELEASE_PATH_PREFIX)
        && !url.contains('?')
        && !has_dot_segment(&full_path)
}

/// A redirect target: https, one of [`REDIRECT_HOSTS`] (or the release path
/// on github.com again), no port, no credentials, no fragment. A query is
/// fine: GitHub's asset hosts use signed URLs.
pub fn redirect_allowed(url: &str) -> bool {
    let Some((host, path)) = split_https(url) else {
        return false;
    };
    if !plain_ascii(url) || path.is_empty() {
        return false;
    }
    let path_only = path.split('?').next().unwrap_or_default();
    if has_dot_segment(path_only) {
        return false;
    }
    if host == RELEASE_HOST {
        return format!("/{path_only}").starts_with(RELEASE_PATH_PREFIX);
    }
    REDIRECT_HOSTS.contains(&host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_table_is_complete_and_well_formed() {
        let release = Release::pinned();
        for platform in [Platform::LinuxAmd64, Platform::LinuxAarch64] {
            for name in [platform.daemon_asset(), platform.cli_asset()] {
                let asset = release.asset(name);
                assert!(asset.is_some(), "{name} is not pinned");
                let (sha, size) = asset
                    .map(|a| (a.sha256.clone(), a.size))
                    .unwrap_or_default();
                // Either a real hash or the explicit fail-closed placeholder.
                assert!(
                    is_real_sha256(&sha) || sha == PLACEHOLDER_SHA256,
                    "{name}: {sha}"
                );
                assert!(size > 0 && size <= MAX_ASSET_BYTES, "{name}: {size}");
            }
        }
        assert!(release.base_url.ends_with(&format!("/v{AGENT_VERSION}/")));
    }

    #[test]
    fn every_pinned_url_passes_the_download_policy() {
        let release = Release::pinned();
        for asset in &release.assets {
            assert!(
                release_url_allowed(&release.url_of(asset)),
                "{}",
                asset.name
            );
        }
    }

    #[test]
    fn platforms() {
        assert_eq!(
            Platform::from_parts("linux", "x86_64"),
            Some(Platform::LinuxAmd64)
        );
        assert_eq!(
            Platform::from_parts("linux", "aarch64"),
            Some(Platform::LinuxAarch64)
        );
        assert_eq!(Platform::from_parts("linux", "arm"), None);
        assert_eq!(Platform::from_parts("windows", "x86_64"), None);
        assert_eq!(Platform::from_parts("macos", "aarch64"), None);
    }

    #[test]
    fn hash_shape() {
        assert!(is_real_sha256(&"ab".repeat(32)));
        assert!(!is_real_sha256(&"AB".repeat(32)));
        assert!(!is_real_sha256(&"ab".repeat(31)));
        assert!(!is_real_sha256(PLACEHOLDER_SHA256));
        assert!(!is_real_sha256(""));
    }

    #[test]
    fn first_request_must_be_the_github_release_path() {
        assert!(release_url_allowed(
            "https://github.com/playit-cloud/playit-agent/releases/download/v1.2.0/playit-linux-amd64"
        ));
        for bad in [
            "http://github.com/playit-cloud/playit-agent/releases/download/v1.2.0/x",
            "https://github.com.evil.com/playit-cloud/playit-agent/releases/download/v1.2.0/x",
            "https://github.com@evil.com/playit-cloud/playit-agent/releases/download/v1.2.0/x",
            "https://github.com:444/playit-cloud/playit-agent/releases/download/v1.2.0/x",
            "https://github.com/evil/playit-agent/releases/download/v1.2.0/x",
            "https://github.com/playit-cloud/playit-agent/releases/download/../../x",
            "https://github.com/playit-cloud/playit-agent/releases/download/v1/x?y=1",
            "https://objects.githubusercontent.com/x",
            "https://playit.gg/download",
            "",
        ] {
            assert!(!release_url_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn redirects_stay_on_github_asset_hosts() {
        for ok in [
            "https://objects.githubusercontent.com/github-production-release-asset/1?X-Amz-Signature=abc&response-content-type=application%2Foctet-stream",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1?sp=r",
            "https://github-releases.githubusercontent.com/1/2?x=y",
            "https://github.com/playit-cloud/playit-agent/releases/download/v1.2.0/x",
        ] {
            assert!(redirect_allowed(ok), "{ok}");
        }
        for bad in [
            "http://objects.githubusercontent.com/x",
            "https://evil.com/objects.githubusercontent.com/x",
            "https://objects.githubusercontent.com.evil.com/x",
            "https://objects.githubusercontent.com@evil.com/x",
            "https://user:pw@objects.githubusercontent.com/x",
            "https://objects.githubusercontent.com:8443/x",
            "https://raw.githubusercontent.com/x",
            "https://github.com/other/repo/releases/download/v1/x",
            "https://playit.gg/x",
            "/relative/path",
            "//objects.githubusercontent.com/x",
            "https://objects.githubusercontent.com/../x",
            "https://objects.githubusercontent.com/x#frag",
            "https://objects.githubusercontent.com/a b",
            "",
        ] {
            assert!(!redirect_allowed(bad), "{bad}");
        }
    }
}
