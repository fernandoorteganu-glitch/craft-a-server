//! The Minecraft versions a server can be created with, for the picker on
//! the create screen.
//!
//! The list comes from the Fabric project's metadata service, which names
//! every game version Fabric knows and whether it is a release. It is kept in
//! memory for a while, and when the service cannot be reached a short built-in
//! list is shown instead, so creating a server never depends on that service.
//! The version a person picks is still checked on create like any other input.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::mods::http::{CappedVec, HttpRequest, Method, Transport};

pub const META_URL: &str = "https://meta.fabricmc.net/v2/versions/game";
/// The list is about 100 KiB; anything near this is not it.
const MAX_BODY: u64 = 1024 * 1024;
const MAX_VERSIONS: usize = 2000;
const FRESH_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// After a failed fetch the built-in list is used for this long.
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// One game version offered in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameVersion {
    pub id: String,
    /// A full release (not a snapshot or pre-release).
    pub stable: bool,
}

/// Used until the first successful fetch, and whenever the service is down.
pub fn fallback() -> Vec<GameVersion> {
    vec![GameVersion {
        id: "26.3".to_owned(),
        stable: true,
    }]
}

#[derive(Deserialize)]
struct Entry {
    version: String,
    #[serde(default)]
    stable: bool,
}

/// Same rule the create call applies, so nothing offered is later refused.
fn acceptable(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
}

/// Reads the service's answer; `None` when it is not a usable list.
pub fn parse(bytes: &[u8]) -> Option<Vec<GameVersion>> {
    let entries: Vec<Entry> = serde_json::from_slice(bytes).ok()?;
    let mut seen = std::collections::HashSet::new();
    let list: Vec<GameVersion> = entries
        .into_iter()
        .filter(|e| acceptable(&e.version) && seen.insert(e.version.clone()))
        .take(MAX_VERSIONS)
        .map(|e| GameVersion {
            id: e.version,
            stable: e.stable,
        })
        .collect();
    (!list.is_empty()).then_some(list)
}

struct Cached {
    at: Instant,
    ok: bool,
    list: Vec<GameVersion>,
}

/// Fetches and remembers the version list.
#[allow(missing_debug_implementations)] // holds a transport
pub struct VersionCatalog {
    transport: Arc<dyn Transport>,
    cache: Mutex<Option<Cached>>,
}

impl VersionCatalog {
    pub fn new(transport: Arc<dyn Transport>) -> VersionCatalog {
        VersionCatalog {
            transport,
            cache: Mutex::new(None),
        }
    }

    /// The versions to offer, newest first. Blocking: run it off the async
    /// threads. Never fails: it falls back to the built-in list.
    pub fn list(&self) -> Vec<GameVersion> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(c) = cache.as_ref() {
            let limit = if c.ok { FRESH_FOR } else { RETRY_AFTER };
            if c.at.elapsed() < limit {
                return c.list.clone();
            }
        }
        match self.fetch() {
            Some(list) => {
                *cache = Some(Cached {
                    at: Instant::now(),
                    ok: true,
                    list: list.clone(),
                });
                list
            }
            None => {
                // Keep an older good list over the built-in one.
                let kept = cache
                    .as_ref()
                    .filter(|c| c.ok)
                    .map_or_else(fallback, |c| c.list.clone());
                *cache = Some(Cached {
                    at: Instant::now(),
                    ok: false,
                    list: kept.clone(),
                });
                kept
            }
        }
    }

    fn fetch(&self) -> Option<Vec<GameVersion>> {
        let req = HttpRequest {
            method: Method::Get,
            url: META_URL.to_owned(),
            body: None,
            max_bytes: MAX_BODY,
        };
        let mut sink = CappedVec::new(MAX_BODY);
        let meta = self.transport.execute(&req, &mut sink).ok()?;
        if meta.status != 200 {
            return None;
        }
        let _ = io::Write::flush(&mut sink);
        parse(&sink.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::testutil::{FakeTransport, Reply};

    fn catalog(net: &FakeTransport) -> VersionCatalog {
        VersionCatalog::new(Arc::new(net.clone()))
    }

    #[test]
    fn parses_releases_and_snapshots_in_order() {
        let list = parse(
            br#"[{"version":"26.4-snapshot-1","stable":false},{"version":"26.3","stable":true},
                 {"version":"26.3","stable":true},{"version":"../x","stable":true},{"version":"1.21.5"}]"#,
        )
        .unwrap_or_default();
        let ids: Vec<_> = list.iter().map(|v| (v.id.as_str(), v.stable)).collect();
        assert_eq!(
            ids,
            [
                ("26.4-snapshot-1", false),
                ("26.3", true),
                ("1.21.5", false)
            ]
        );
    }

    #[test]
    fn junk_is_not_a_list() {
        assert!(parse(b"not json").is_none());
        assert!(parse(b"[]").is_none());
        assert!(parse(br#"[{"version":"a b"}]"#).is_none());
    }

    #[test]
    fn fetches_once_then_serves_from_memory() {
        let net = FakeTransport::new();
        net.route(
            META_URL,
            vec![Reply::Json(
                br#"[{"version":"26.3","stable":true}]"#.to_vec(),
            )],
        );
        let c = catalog(&net);
        assert_eq!(c.list().len(), 1);
        assert_eq!(c.list().len(), 1);
        assert_eq!(net.requests().len(), 1);
    }

    #[test]
    fn falls_back_when_the_service_is_down_and_does_not_hammer_it() {
        let net = FakeTransport::new();
        net.route(META_URL, vec![Reply::NetworkError]);
        let c = catalog(&net);
        assert_eq!(c.list(), fallback());
        assert_eq!(c.list(), fallback());
        assert_eq!(net.requests().len(), 1);
    }

    #[test]
    fn an_error_status_or_a_flood_falls_back() {
        for reply in [Reply::Status(500), Reply::Flood(2 * 1024 * 1024)] {
            let net = FakeTransport::new();
            net.route(META_URL, vec![reply]);
            assert_eq!(catalog(&net).list(), fallback());
        }
    }
}
