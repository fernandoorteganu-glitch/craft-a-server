//! The Modrinth client: typed calls over a [`Transport`], with the rate
//! limiter, retries and the download policy (CDN host only, checksum, size,
//! structural jar check).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::RngExt;
use serde::de::DeserializeOwned;

use super::error::ModError;
use super::files::{HashingFile, MAX_MOD_BYTES, check_fabric_jar};
use super::http::{
    API_BASE, BURST, CappedVec, HttpError, HttpRequest, Method, REQUESTS_PER_SECOND, RateLimiter,
    Transport, validate_cdn_url,
};
use super::model::{Project, SearchHit, SearchPage, Version, valid_id};
use super::resolver::{PlannedFile, VersionSource};

const MAX_RETRIES: u32 = 3;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
/// Search results and single objects are small; version lists carry changelogs.
const MAX_SMALL_BODY: u64 = 2 * 1024 * 1024;
const MAX_LIST_BODY: u64 = 8 * 1024 * 1024;
const PROJECT_BATCH: usize = 40;
/// Icons are small; a bigger "icon" is not one.
const MAX_ICON_BYTES: u64 = 512 * 1024;

/// What to search for.
#[derive(Debug, Clone, Copy)]
pub struct SearchQuery<'a> {
    pub text: &'a str,
    pub mc_version: &'a str,
    /// Also list mods whose server side is `unknown`.
    pub include_unknown: bool,
    pub offset: u32,
    pub limit: u32,
    /// Only mods in this Modrinth category (one of [`CATEGORIES`]).
    pub category: Option<&'a str>,
    pub sort: SearchSort,
}

/// How results are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSort {
    /// Best match for the text, or most downloaded when there is no text.
    #[default]
    Auto,
    Downloads,
    Follows,
    Newest,
    Updated,
}

impl SearchSort {
    /// Parses the value used in the panel's links; unknown values mean `Auto`.
    pub fn parse(v: &str) -> SearchSort {
        match v {
            "downloads" => SearchSort::Downloads,
            "follows" => SearchSort::Follows,
            "newest" => SearchSort::Newest,
            "updated" => SearchSort::Updated,
            _ => SearchSort::Auto,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            SearchSort::Auto => "auto",
            SearchSort::Downloads => "downloads",
            SearchSort::Follows => "follows",
            SearchSort::Newest => "newest",
            SearchSort::Updated => "updated",
        }
    }

    fn index(self, has_text: bool) -> &'static str {
        match self {
            SearchSort::Auto if has_text => "relevance",
            SearchSort::Auto | SearchSort::Downloads => "downloads",
            SearchSort::Follows => "follows",
            SearchSort::Newest => "newest",
            SearchSort::Updated => "updated",
        }
    }
}

/// Modrinth categories offered as filters (a fixed list: the value goes into
/// a search facet, so nothing else is accepted).
pub const CATEGORIES: &[&str] = &[
    "adventure",
    "decoration",
    "economy",
    "equipment",
    "food",
    "game-mechanics",
    "library",
    "magic",
    "management",
    "minigame",
    "mobs",
    "optimization",
    "social",
    "storage",
    "technology",
    "transportation",
    "utility",
    "worldgen",
];

type Sleeper = Box<dyn Fn(Duration) + Send + Sync>;

#[allow(missing_debug_implementations)] // holds a transport and a closure
pub struct ModrinthClient {
    transport: Arc<dyn Transport>,
    limiter: RateLimiter,
    sleeper: Sleeper,
}

impl ModrinthClient {
    pub fn new(transport: Arc<dyn Transport>) -> ModrinthClient {
        ModrinthClient {
            transport,
            limiter: RateLimiter::new(REQUESTS_PER_SECOND, BURST),
            sleeper: Box::new(std::thread::sleep),
        }
    }

    /// Replaces the sleep function (tests do not wait).
    #[must_use]
    pub fn with_sleeper(mut self, sleeper: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleeper = Box::new(sleeper);
        self
    }

    fn pause(&self, d: Duration) {
        if !d.is_zero() {
            (self.sleeper)(d);
        }
    }

    fn wait_for_slot(&self) {
        self.pause(self.limiter.reserve(Instant::now()));
    }

    fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
        let base = retry_after.map_or_else(
            || Duration::from_secs(1_u64 << attempt.min(5)),
            Duration::from_secs,
        );
        let jitter = Duration::from_millis(rand::rng().random_range(0..500));
        base.min(MAX_RETRY_WAIT) + jitter
    }

    /// Sends a request to the Modrinth API; `None` for a 404.
    fn api<T: DeserializeOwned>(
        &self,
        method: Method,
        path_and_query: &str,
        body: Option<Vec<u8>>,
        max_bytes: u64,
    ) -> Result<Option<T>, ModError> {
        let req = HttpRequest {
            method,
            url: format!("{API_BASE}{path_and_query}"),
            body,
            max_bytes,
        };
        let mut attempt = 0;
        loop {
            self.wait_for_slot();
            let mut sink = CappedVec::new(max_bytes);
            match self.transport.execute(&req, &mut sink) {
                Ok(meta) => {
                    if meta.ratelimit_remaining.is_some_and(|r| r < 10) {
                        // Close to the per-minute limit: back off for a moment.
                        self.pause(Duration::from_secs(2));
                    }
                    match meta.status {
                        200 => {
                            return serde_json::from_slice(&sink.bytes)
                                .map(Some)
                                .map_err(|_| ModError::BadResponse("invalid JSON".into()));
                        }
                        404 => return Ok(None),
                        429 | 500..=599 if attempt < MAX_RETRIES => {
                            self.pause(Self::backoff(attempt, meta.retry_after_secs));
                            attempt += 1;
                        }
                        status => return Err(HttpError::Status(status).into()),
                    }
                }
                Err(HttpError::Network(_)) if attempt < MAX_RETRIES => {
                    self.pause(Self::backoff(attempt, None));
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Searches Fabric mods that run on a server for the given Minecraft version.
    pub fn search(&self, q: SearchQuery<'_>) -> Result<Vec<SearchHit>, ModError> {
        check_mc_version(q.mc_version)?;
        let server_side: Vec<&str> = if q.include_unknown {
            vec!["server_side!=unsupported"]
        } else {
            vec!["server_side:required", "server_side:optional"]
        };
        let mut facets = vec![
            vec!["project_type:mod".to_owned()],
            vec!["categories:fabric".to_owned()],
            vec![format!("versions:{}", q.mc_version)],
            server_side.iter().map(|s| (*s).to_owned()).collect(),
        ];
        if let Some(cat) = q.category.filter(|c| CATEGORIES.contains(c)) {
            facets.push(vec![format!("categories:{cat}")]);
        }
        let facets = serde_json::json!(facets);
        let text: String = q
            .text
            .chars()
            .filter(|c| !c.is_control())
            .take(100)
            .collect();
        let query = serde_urlencoded::to_string([
            ("query", text.as_str()),
            ("facets", facets.to_string().as_str()),
            ("index", q.sort.index(!text.trim().is_empty())),
            ("limit", q.limit.clamp(1, 50).to_string().as_str()),
            ("offset", q.offset.min(10_000).to_string().as_str()),
        ])
        .map_err(|_| ModError::invalid("the search text could not be used"))?;
        let page: SearchPage = self
            .api(
                Method::Get,
                &format!("/search?{query}"),
                None,
                MAX_SMALL_BODY,
            )?
            .ok_or_else(|| ModError::BadResponse("the search endpoint was not found".into()))?;
        // The facets already filter; check again because the answer is untrusted.
        Ok(page
            .hits
            .into_iter()
            .filter(|h| {
                valid_id(&h.project_id) && h.project_type == "mod" && h.server_side != "unsupported"
            })
            .collect())
    }

    /// Downloads a project icon from the Modrinth CDN: the URL is checked
    /// first, the size is capped, and only PNG, JPEG, GIF and WebP come back
    /// (with the content type that goes with what the bytes really are).
    pub fn icon(&self, url: &str) -> Result<Option<(Vec<u8>, &'static str)>, ModError> {
        if !validate_cdn_url(url) {
            return Err(ModError::invalid("that is not a Modrinth image address"));
        }
        let req = HttpRequest {
            method: Method::Get,
            url: url.to_owned(),
            body: None,
            max_bytes: MAX_ICON_BYTES,
        };
        let mut sink = CappedVec::new(MAX_ICON_BYTES);
        let meta = self.transport.execute(&req, &mut sink)?;
        if meta.status != 200 {
            return Ok(None);
        }
        Ok(image_type(&sink.bytes).map(|t| (sink.bytes, t)))
    }

    pub fn project(&self, id_or_slug: &str) -> Result<Option<Project>, ModError> {
        check_id(id_or_slug)?;
        self.api(
            Method::Get,
            &format!("/project/{id_or_slug}"),
            None,
            MAX_SMALL_BODY,
        )
    }

    /// Several projects at once (for titles); unknown ids are simply missing.
    pub fn projects(&self, ids: &[String]) -> Result<Vec<Project>, ModError> {
        let mut out = Vec::new();
        for chunk in ids.chunks(PROJECT_BATCH) {
            let valid: Vec<&String> = chunk.iter().filter(|i| valid_id(i)).collect();
            if valid.is_empty() {
                continue;
            }
            let query = serde_urlencoded::to_string([(
                "ids",
                serde_json::json!(valid).to_string().as_str(),
            )])
            .map_err(|_| ModError::invalid("bad project ids"))?;
            let got: Option<Vec<Project>> = self.api(
                Method::Get,
                &format!("/projects?{query}"),
                None,
                MAX_LIST_BODY,
            )?;
            out.extend(
                got.unwrap_or_default()
                    .into_iter()
                    .filter(|p| valid_id(&p.id)),
            );
        }
        Ok(out)
    }

    pub fn project_versions(
        &self,
        project_id: &str,
        mc_version: &str,
    ) -> Result<Vec<Version>, ModError> {
        check_id(project_id)?;
        check_mc_version(mc_version)?;
        let query = serde_urlencoded::to_string([
            ("loaders", r#"["fabric"]"#),
            (
                "game_versions",
                serde_json::json!([mc_version]).to_string().as_str(),
            ),
        ])
        .map_err(|_| ModError::invalid("bad version filter"))?;
        let got: Option<Vec<Version>> = self.api(
            Method::Get,
            &format!("/project/{project_id}/version?{query}"),
            None,
            MAX_LIST_BODY,
        )?;
        Ok(got.unwrap_or_default())
    }

    pub fn version(&self, version_id: &str) -> Result<Option<Version>, ModError> {
        check_id(version_id)?;
        self.api(
            Method::Get,
            &format!("/version/{version_id}"),
            None,
            MAX_SMALL_BODY,
        )
    }

    /// For each installed file hash, the newest matching version (one request).
    pub fn latest_for_hashes(
        &self,
        sha512_hashes: &[String],
        mc_version: &str,
    ) -> Result<HashMap<String, Version>, ModError> {
        check_mc_version(mc_version)?;
        if sha512_hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let body = serde_json::json!({
            "hashes": sha512_hashes,
            "algorithm": "sha512",
            "loaders": ["fabric"],
            "game_versions": [mc_version],
        })
        .to_string()
        .into_bytes();
        let got: Option<HashMap<String, Version>> = self.api(
            Method::Post,
            "/version_files/update",
            Some(body),
            MAX_LIST_BODY,
        )?;
        Ok(got.unwrap_or_default())
    }

    /// Downloads `file` into `dir` under its sanitized name and returns the path.
    ///
    /// Policy, in order: the URL must be `https://cdn.modrinth.com/...`; the
    /// body is capped at the announced size; the SHA-512 must match what the
    /// API published; the file must be a zip that names `fabric.mod.json`. The
    /// bytes are written to a `.part` file that is renamed only after all of
    /// that passed, and removed otherwise.
    pub fn download(&self, file: &PlannedFile, dir: &Path) -> Result<PathBuf, ModError> {
        if !validate_cdn_url(&file.url) {
            return Err(ModError::ForbiddenHost(
                file.url
                    .split("://")
                    .nth(1)
                    .and_then(|r| r.split('/').next())
                    .unwrap_or("an unknown host")
                    .chars()
                    .filter(char::is_ascii_graphic)
                    .take(60)
                    .collect(),
            ));
        }
        if file.size == 0 || file.size > MAX_MOD_BYTES {
            return Err(ModError::TooLarge {
                file: file.file_name.clone(),
                limit_bytes: MAX_MOD_BYTES,
            });
        }
        let part = dir.join(format!("{}.part", file.file_name));
        let done = dir.join(&file.file_name);
        let result = self.download_to(file, &part);
        match result {
            Ok(()) => std::fs::rename(&part, &done).map_err(|e| {
                tracing::warn!(error = %e, "could not move a verified download into place");
                let _ = std::fs::remove_file(&part);
                storage_error()
            })?,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
        }
        Ok(done)
    }

    fn download_to(&self, file: &PlannedFile, part: &Path) -> Result<(), ModError> {
        let mut writer = HashingFile::create(part, file.size).map_err(|e| {
            tracing::warn!(error = %e, "cannot create the download file");
            storage_error()
        })?;
        let req = HttpRequest {
            method: Method::Get,
            url: file.url.clone(),
            body: None,
            max_bytes: file.size,
        };
        let mut attempt = 0;
        let meta = loop {
            self.wait_for_slot();
            match self.transport.execute(&req, &mut writer) {
                Ok(meta) => break meta,
                // A partial body cannot be resumed through the sink; only a
                // failure before any byte arrived is retried.
                Err(HttpError::Network(_)) if attempt < MAX_RETRIES && writer_is_empty(part) => {
                    self.pause(Self::backoff(attempt, None));
                    attempt += 1;
                }
                Err(HttpError::TooLarge(_)) => {
                    return Err(ModError::TooLarge {
                        file: file.file_name.clone(),
                        limit_bytes: file.size,
                    });
                }
                Err(HttpError::Sink(why)) => {
                    return Err(if why.contains("size limit") {
                        ModError::TooLarge {
                            file: file.file_name.clone(),
                            limit_bytes: file.size,
                        }
                    } else {
                        tracing::warn!(error = %why, "cannot store the download");
                        storage_error()
                    });
                }
                Err(e) => return Err(e.into()),
            }
        };
        if meta.status != 200 {
            return Err(HttpError::Status(meta.status).into());
        }
        let (sha512, written) = writer.finish().map_err(|e| {
            tracing::warn!(error = %e, "cannot finish the download file");
            storage_error()
        })?;
        if written != file.size {
            return Err(ModError::SizeMismatch {
                file: file.file_name.clone(),
                expected: file.size,
                got: written,
            });
        }
        if sha512 != file.sha512 {
            return Err(ModError::HashMismatch {
                file: file.file_name.clone(),
            });
        }
        check_fabric_jar(part).map_err(|why| ModError::BadFile {
            file: file.file_name.clone(),
            why,
        })
    }
}

fn writer_is_empty(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() == 0)
}

fn storage_error() -> ModError {
    ModError::BadResponse("the downloaded file could not be stored".into())
}

fn check_id(id: &str) -> Result<(), ModError> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(ModError::invalid("that is not a valid Modrinth project id"))
    }
}

/// Minecraft versions are `26.3`, `1.21.11`, `24w14a`...: letters, digits, `.`, `_`, `-`.
/// The content type of an image from its first bytes, for the formats a
/// browser draws safely in an `<img>`. SVG is left out on purpose.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.get(..4) == Some(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn check_mc_version(v: &str) -> Result<(), ModError> {
    let ok = !v.is_empty()
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(ModError::invalid("invalid Minecraft version"))
    }
}

/// Adapts the client to the resolver for one Minecraft version.
#[allow(missing_debug_implementations)]
pub struct ClientSource<'a> {
    pub client: &'a ModrinthClient,
    pub mc_version: &'a str,
}

impl VersionSource for ClientSource<'_> {
    fn project_versions(&self, project_id: &str) -> Result<Vec<Version>, ModError> {
        self.client.project_versions(project_id, self.mc_version)
    }

    fn version(&self, version_id: &str) -> Result<Option<Version>, ModError> {
        self.client.version(version_id)
    }
}

/// `craft-a-server/<version> (+<repo>; <contact>)`: identifies the program,
/// never just the HTTP library.
pub fn user_agent(contact: Option<&str>) -> String {
    let base = format!(
        "craft-a-server/{} (+{}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    );
    match contact {
        Some(c) if !c.trim().is_empty() => format!("{base}; {})", c.trim()),
        _ => format!("{base})"),
    }
}

#[cfg(test)]
mod tests;
