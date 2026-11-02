//! The tunnel's HTTP layer behind a trait, plus the download policy.
//!
//! [`HttpFetch`] is the only thing here that touches the network. Production
//! uses [`UreqFetch`] (rustls, https only, no proxy, **no automatic
//! redirects**); tests use fakes, so no test ever reaches GitHub or playit.
//!
//! Redirects are followed by [`download`], one hop at a time, and every
//! target must pass [`redirect_allowed`]: a release asset may move between
//! GitHub's own hosts and nowhere else.

use std::io::{self, Read, Write};
use std::time::Duration;

use super::release::{MAX_REDIRECTS, redirect_allowed, release_url_allowed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMethod {
    Get,
    Post,
}

/// One request. `authorization` carries a secret, so `Debug` hides it.
#[derive(Clone)]
pub struct FetchRequest {
    pub method: FetchMethod,
    pub url: String,
    /// Full value of the `Authorization` header.
    pub authorization: Option<String>,
    /// JSON body for `Post`.
    pub body: Option<Vec<u8>>,
    /// The response body may not be longer than this.
    pub max_bytes: u64,
}

impl std::fmt::Debug for FetchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<hidden>"),
            )
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchMeta {
    pub status: u16,
    /// `Location` of a 3xx answer (the body of a redirect is not read).
    pub location: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    #[error("network error: {0}")]
    Network(String),
    #[error("the response is larger than the allowed {0} bytes")]
    TooLarge(u64),
    #[error("could not store the response: {0}")]
    Sink(String),
}

/// Performs one request and streams the body into `sink`. Implementations
/// must not follow redirects and must stop after `req.max_bytes`.
pub trait HttpFetch: Send + Sync {
    fn fetch(&self, req: &FetchRequest, sink: &mut dyn Write) -> Result<FetchMeta, FetchError>;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DownloadError {
    #[error("the download address is not allowed")]
    UrlNotAllowed,
    #[error("the download was redirected somewhere that is not allowed")]
    RedirectNotAllowed,
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("the download server answered HTTP {0}")]
    Status(u16),
    #[error("a redirect came without a target")]
    RedirectWithoutTarget,
    #[error(transparent)]
    Fetch(#[from] FetchError),
}

/// A sink that errors once more than `max` bytes arrive.
#[derive(Debug)]
struct CappedVec {
    bytes: Vec<u8>,
    max: u64,
}

impl Write for CappedVec {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if (self.bytes.len() as u64).saturating_add(buf.len() as u64) > self.max {
            return Err(io::Error::other("response body over the size cap"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Downloads `url` (a release asset) into memory, following at most
/// [`MAX_REDIRECTS`] redirects that pass the policy, never more than
/// `max_bytes`.
pub fn download(
    fetch: &dyn HttpFetch,
    url: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, DownloadError> {
    if !release_url_allowed(url) {
        return Err(DownloadError::UrlNotAllowed);
    }
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let mut sink = CappedVec {
            bytes: Vec::new(),
            max: max_bytes,
        };
        let req = FetchRequest {
            method: FetchMethod::Get,
            url: current.clone(),
            authorization: None,
            body: None,
            max_bytes,
        };
        let meta = fetch.fetch(&req, &mut sink).map_err(|e| match e {
            // The cap is enforced by our sink too; report it as a size error.
            FetchError::Sink(_) => FetchError::TooLarge(max_bytes),
            other => other,
        })?;
        if matches!(meta.status, 301 | 302 | 303 | 307 | 308) {
            let target = meta.location.ok_or(DownloadError::RedirectWithoutTarget)?;
            if !redirect_allowed(&target) {
                return Err(DownloadError::RedirectNotAllowed);
            }
            current = target;
            continue;
        }
        if meta.status != 200 {
            return Err(DownloadError::Status(meta.status));
        }
        return Ok(sink.bytes);
    }
    Err(DownloadError::TooManyRedirects)
}

// ------------------------------------------------------------------- ureq

/// The real transport: HTTPS only, no proxy, no redirects, bounded bodies.
#[derive(Debug)]
pub struct UreqFetch {
    agent: ureq::Agent,
}

impl UreqFetch {
    pub fn new(user_agent: &str) -> UreqFetch {
        Self::build(user_agent, true)
    }

    fn build(user_agent: &str, https_only: bool) -> UreqFetch {
        let config = ureq::Agent::config_builder()
            .https_only(https_only)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .proxy(None)
            .user_agent(user_agent)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .build();
        UreqFetch {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// Plain-HTTP variant for tests against a loopback server.
    #[cfg(test)]
    pub fn for_loopback_tests() -> UreqFetch {
        Self::build("craft-a-server-test/0", false)
    }
}

fn total_timeout(max_bytes: u64) -> Duration {
    if max_bytes > 1024 * 1024 {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    }
}

impl HttpFetch for UreqFetch {
    fn fetch(&self, req: &FetchRequest, sink: &mut dyn Write) -> Result<FetchMeta, FetchError> {
        let timeout = Some(total_timeout(req.max_bytes));
        let result = match req.method {
            FetchMethod::Get => {
                let mut r = self
                    .agent
                    .get(&req.url)
                    .config()
                    .timeout_global(timeout)
                    .build();
                if let Some(auth) = &req.authorization {
                    r = r.header("Authorization", auth);
                }
                r.call()
            }
            FetchMethod::Post => {
                let mut r = self
                    .agent
                    .post(&req.url)
                    .config()
                    .timeout_global(timeout)
                    .build();
                r = r.header("Content-Type", "application/json");
                if let Some(auth) = &req.authorization {
                    r = r.header("Authorization", auth);
                }
                r.send(req.body.as_deref().unwrap_or_default())
            }
        };
        let mut response = result.map_err(|e| FetchError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            return Ok(FetchMeta { status, location });
        }
        let announced = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if let Some(len) = announced
            && len > req.max_bytes
        {
            return Err(FetchError::TooLarge(req.max_bytes));
        }
        copy_bounded(&mut response.body_mut().as_reader(), sink, req.max_bytes)?;
        Ok(FetchMeta {
            status,
            location: None,
        })
    }
}

/// Copies at most `max` bytes; a longer body is an error, not a truncation.
fn copy_bounded(reader: &mut dyn Read, sink: &mut dyn Write, max: u64) -> Result<u64, FetchError> {
    let mut buf = [0_u8; 32 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| FetchError::Network(e.to_string()))?;
        if n == 0 {
            return Ok(total);
        }
        total = total.saturating_add(n as u64);
        if total > max {
            return Err(FetchError::TooLarge(max));
        }
        let chunk = buf.get(..n).unwrap_or_default();
        sink.write_all(chunk)
            .map_err(|e| FetchError::Sink(e.to_string()))?;
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted `HttpFetch` for tests: URL -> canned answers.

    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Clone)]
    pub enum Canned {
        Ok(Vec<u8>),
        Redirect(Option<String>),
        Status(u16),
        Network,
    }

    /// Serves canned answers per URL; records every request it saw.
    #[derive(Debug, Default)]
    pub struct FakeFetch {
        routes: Mutex<HashMap<String, Canned>>,
        seen: Mutex<Vec<FetchRequest>>,
    }

    impl FakeFetch {
        pub fn route(&self, url: &str, answer: Canned) {
            self.routes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(url.to_owned(), answer);
        }

        pub fn requests(&self) -> Vec<FetchRequest> {
            self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl HttpFetch for FakeFetch {
        fn fetch(&self, req: &FetchRequest, sink: &mut dyn Write) -> Result<FetchMeta, FetchError> {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(req.clone());
            let answer = self
                .routes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&req.url)
                .cloned();
            match answer {
                Some(Canned::Ok(bytes)) => {
                    if bytes.len() as u64 > req.max_bytes {
                        return Err(FetchError::TooLarge(req.max_bytes));
                    }
                    sink.write_all(&bytes)
                        .map_err(|e| FetchError::Sink(e.to_string()))?;
                    Ok(FetchMeta {
                        status: 200,
                        location: None,
                    })
                }
                Some(Canned::Redirect(location)) => Ok(FetchMeta {
                    status: 302,
                    location,
                }),
                Some(Canned::Status(status)) => Ok(FetchMeta {
                    status,
                    location: None,
                }),
                Some(Canned::Network) | None => Err(FetchError::Network("no route".into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::fake::{Canned, FakeFetch};
    use super::*;

    const START: &str =
        "https://github.com/playit-cloud/playit-agent/releases/download/v1.2.0/playit-linux-amd64";
    const CDN: &str = "https://release-assets.githubusercontent.com/asset/1?sp=r";

    #[test]
    fn follows_an_allowed_redirect_and_returns_the_bytes() {
        let f = FakeFetch::default();
        f.route(START, Canned::Redirect(Some(CDN.into())));
        f.route(CDN, Canned::Ok(b"binary".to_vec()));
        assert_eq!(download(&f, START, 100), Ok(b"binary".to_vec()));
        assert_eq!(f.requests().len(), 2);
    }

    #[test]
    fn refuses_a_redirect_off_the_github_hosts_without_requesting_it() {
        let f = FakeFetch::default();
        f.route(
            START,
            Canned::Redirect(Some("https://evil.example/x".into())),
        );
        f.route("https://evil.example/x", Canned::Ok(b"evil".to_vec()));
        assert_eq!(
            download(&f, START, 100),
            Err(DownloadError::RedirectNotAllowed)
        );
        assert_eq!(
            f.requests().len(),
            1,
            "the evil host must never be contacted"
        );
    }

    #[test]
    fn refuses_plain_http_and_missing_targets_and_loops() {
        let f = FakeFetch::default();
        f.route(
            START,
            Canned::Redirect(Some("http://objects.githubusercontent.com/x".into())),
        );
        assert_eq!(
            download(&f, START, 100),
            Err(DownloadError::RedirectNotAllowed)
        );
        f.route(START, Canned::Redirect(None));
        assert_eq!(
            download(&f, START, 100),
            Err(DownloadError::RedirectWithoutTarget)
        );
        f.route(START, Canned::Redirect(Some(CDN.into())));
        f.route(CDN, Canned::Redirect(Some(CDN.into())));
        assert_eq!(
            download(&f, START, 100),
            Err(DownloadError::TooManyRedirects)
        );
    }

    #[test]
    fn refuses_a_start_url_outside_the_release_path() {
        let f = FakeFetch::default();
        assert_eq!(
            download(&f, "https://playit.gg/binary", 100),
            Err(DownloadError::UrlNotAllowed)
        );
        assert!(f.requests().is_empty());
    }

    #[test]
    fn oversize_and_error_statuses_are_errors() {
        let f = FakeFetch::default();
        f.route(START, Canned::Ok(vec![0; 200]));
        assert!(matches!(
            download(&f, START, 100),
            Err(DownloadError::Fetch(FetchError::TooLarge(100)))
        ));
        f.route(START, Canned::Status(404));
        assert_eq!(download(&f, START, 100), Err(DownloadError::Status(404)));
        f.route(START, Canned::Network);
        assert!(matches!(
            download(&f, START, 100),
            Err(DownloadError::Fetch(FetchError::Network(_)))
        ));
    }

    #[test]
    fn request_debug_hides_the_authorization() {
        let r = FetchRequest {
            method: FetchMethod::Post,
            url: "https://example.invalid/".into(),
            authorization: Some("agent-key supersecret".into()),
            body: None,
            max_bytes: 1,
        };
        assert!(!format!("{r:?}").contains("supersecret"));
    }

    // ---- the real transport, against a loopback server

    /// Answers the first request. With `echo_auth` the body says whether an
    /// `Authorization: agent-key abc` header arrived.
    fn serve(response: Option<Vec<u8>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").map(|l| (l.local_addr(), l));
        let (addr, listener) = match listener {
            Ok((Ok(addr), l)) => (addr, l),
            _ => return 0,
        };
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0_u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request =
                    String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).to_ascii_lowercase();
                let out = response.unwrap_or_else(|| {
                    let body = if request.contains("authorization: agent-key abc") {
                        "auth:yes"
                    } else {
                        "auth:no"
                    };
                    format!(
                        "HTTP/1.1 200 OK
Content-Length: {}
Connection: close

{body}",
                        body.len()
                    )
                    .into_bytes()
                });
                let _ = stream.write_all(&out);
            }
        });
        addr.port()
    }

    #[test]
    fn real_transport_returns_location_without_following_it() {
        let port = serve(Some(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/evil\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        ));
        let t = UreqFetch::for_loopback_tests();
        let mut sink = Vec::new();
        let meta = t.fetch(
            &FetchRequest {
                method: FetchMethod::Get,
                url: format!("http://127.0.0.1:{port}/x"),
                authorization: None,
                body: None,
                max_bytes: 100,
            },
            &mut sink,
        );
        assert_eq!(
            meta,
            Ok(FetchMeta {
                status: 302,
                location: Some("http://127.0.0.1:1/evil".into())
            })
        );
        assert!(sink.is_empty());
    }

    #[test]
    fn real_transport_sends_the_authorization_header_and_caps_the_body() {
        let port = serve(None);
        let t = UreqFetch::for_loopback_tests();
        let mut sink = Vec::new();
        let meta = t.fetch(
            &FetchRequest {
                method: FetchMethod::Post,
                url: format!("http://127.0.0.1:{port}/x"),
                authorization: Some("agent-key abc".into()),
                body: Some(b"{}".to_vec()),
                max_bytes: 100,
            },
            &mut sink,
        );
        assert_eq!(meta.map(|m| m.status), Ok(200));
        assert_eq!(sink, b"auth:yes");

        let port = serve(Some(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nabc".to_vec(),
        ));
        let mut sink = Vec::new();
        let r = t.fetch(
            &FetchRequest {
                method: FetchMethod::Get,
                url: format!("http://127.0.0.1:{port}/x"),
                authorization: None,
                body: None,
                max_bytes: 10,
            },
            &mut sink,
        );
        assert_eq!(r, Err(FetchError::TooLarge(10)));
    }

    #[test]
    fn real_transport_is_https_only_in_production_mode() {
        let t = UreqFetch::new("craft-a-server-test/0");
        let mut sink = Vec::new();
        let r = t.fetch(
            &FetchRequest {
                method: FetchMethod::Get,
                url: "http://127.0.0.1:9/x".into(),
                authorization: None,
                body: None,
                max_bytes: 10,
            },
            &mut sink,
        );
        assert!(matches!(r, Err(FetchError::Network(_))), "{r:?}");
    }
}
