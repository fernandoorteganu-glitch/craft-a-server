//! The Minecraft versions a server can be created with, for the picker on
//! the create screen.
//!
//! The list comes from the Fabric project's metadata service, which names
//! every game version Fabric knows and whether it is a release. It is kept in
//! memory for a while, and when the service cannot be reached a short built-in
//! list is shown instead, so creating a server never depends on that service.
//! The version a person picks is still checked on create like any other input.

use std::io;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::mods::http::{CappedVec, HttpRequest, Method, Transport};

pub const META_URL: &str = "https://meta.fabricmc.net/v2/versions/game";
/// The list is about 100 KiB; anything near this is not it.
const MAX_BODY: u64 = 1024 * 1024;
const MAX_VERSIONS: usize = 2000;
const FRESH_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// The first call waits this long for the service before showing the
/// built-in list (the fetch carries on and fills the cache).
const FIRST_WAIT: Duration = Duration::from_secs(3);
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

#[derive(Default)]
struct State {
    cache: Option<Cached>,
    /// A fetch is running on its own thread.
    refreshing: bool,
}

struct Inner {
    transport: Arc<dyn Transport>,
    state: Mutex<State>,
}

/// Fetches and remembers the version list.
///
/// A person is waiting on the create page, so the catalog never makes them
/// wait for the network for long: the fetch runs on its own thread, the first
/// caller waits a few seconds at most, and an out-of-date list is served while
/// a fresh one is fetched in the background.
#[allow(missing_debug_implementations)] // holds a transport
pub struct VersionCatalog {
    inner: Arc<Inner>,
    wait: Duration,
}

impl VersionCatalog {
    pub fn new(transport: Arc<dyn Transport>) -> VersionCatalog {
        VersionCatalog {
            inner: Arc::new(Inner {
                transport,
                state: Mutex::new(State::default()),
            }),
            wait: FIRST_WAIT,
        }
    }

    /// How long the very first call waits for the service (tests shorten it).
    #[must_use]
    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// The versions to offer, newest first. Blocking for at most the wait
    /// given to [`Self::with_wait`]: run it off the async threads. Never
    /// fails: it falls back to the built-in list.
    pub fn list(&self) -> Vec<GameVersion> {
        let rx = {
            let mut st = self
                .inner
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(c) = st.cache.as_ref() {
                let limit = if c.ok { FRESH_FOR } else { RETRY_AFTER };
                let list = c.list.clone();
                if c.at.elapsed() >= limit && !st.refreshing {
                    st.refreshing = true;
                    drop(st);
                    self.spawn_refresh(None);
                }
                return list;
            }
            if st.refreshing {
                // The first fetch is still running for someone else.
                return fallback();
            }
            st.refreshing = true;
            drop(st);
            let (tx, rx) = mpsc::channel();
            self.spawn_refresh(Some(tx));
            rx
        };
        rx.recv_timeout(self.wait).unwrap_or_else(|_| fallback())
    }

    fn spawn_refresh(&self, reply: Option<mpsc::Sender<Vec<GameVersion>>>) {
        let inner = Arc::clone(&self.inner);
        let started = std::thread::Builder::new()
            .name("game-versions".to_owned())
            .spawn({
                let inner = Arc::clone(&inner);
                move || {
                    let list = inner.refresh();
                    if let Some(tx) = reply {
                        let _ = tx.send(list);
                    }
                }
            });
        if started.is_err() {
            // No thread to fetch on: remember that, and serve the built-in list.
            inner.record(None);
        }
    }
}

impl Inner {
    fn refresh(&self) -> Vec<GameVersion> {
        let fetched = self.fetch();
        self.record(fetched)
    }

    /// Stores the outcome of a fetch and returns the list to serve.
    fn record(&self, fetched: Option<Vec<GameVersion>>) -> Vec<GameVersion> {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.refreshing = false;
        let (ok, list) = match fetched {
            Some(list) => (true, list),
            // Keep an older good list over the built-in one.
            None => (
                false,
                st.cache
                    .as_ref()
                    .filter(|c| c.ok)
                    .map_or_else(fallback, |c| c.list.clone()),
            ),
        };
        st.cache = Some(Cached {
            at: Instant::now(),
            ok,
            list: list.clone(),
        });
        list
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

    /// Answers only once the gate is opened: a service that hangs.
    struct Hanging {
        open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl Transport for Hanging {
        fn execute(
            &self,
            _req: &HttpRequest,
            sink: &mut dyn io::Write,
        ) -> Result<crate::mods::http::HttpMeta, crate::mods::http::HttpError> {
            let (lock, cv) = &*self.open;
            let mut open = lock.lock().unwrap_or_else(PoisonError::into_inner);
            while !*open {
                open = cv.wait(open).unwrap_or_else(PoisonError::into_inner);
            }
            let _ = sink.write_all(br#"[{"version":"27.1","stable":true}]"#);
            Ok(crate::mods::http::HttpMeta {
                status: 200,
                retry_after_secs: None,
                ratelimit_remaining: None,
            })
        }
    }

    #[test]
    fn a_hanging_service_does_not_hold_the_page_up() {
        let open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let c = VersionCatalog::new(Arc::new(Hanging { open: open.clone() }))
            .with_wait(Duration::from_millis(50));
        let started = Instant::now();
        assert_eq!(c.list(), fallback());
        // A second caller while the fetch is still out does not wait at all.
        assert_eq!(c.list(), fallback());
        assert!(started.elapsed() < Duration::from_secs(2));

        // The service wakes up; the fetch that was left running fills the cache.
        *open.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
        open.1.notify_all();
        let mut got = Vec::new();
        for _ in 0..200 {
            got = c.list();
            if got.first().is_some_and(|v| v.id == "27.1") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got.first().map(|v| v.id.as_str()), Some("27.1"));
    }
}
