//! The HTTP layer behind a trait, plus the policy that keeps a hostile API
//! response from sending us anywhere else.
//!
//! * [`Transport`] is the only thing that touches the network. Production uses
//!   [`UreqTransport`] (rustls, no redirects, no proxy, bounded bodies); tests
//!   use a fake that serves the recorded fixtures.
//! * Policy that must hold for every transport lives outside it: download URLs
//!   are checked by [`validate_cdn_url`] before any request, and the byte caps
//!   are enforced again by the sinks the client hands to the transport.
//! * [`RateLimiter`] keeps us far under Modrinth's 300 requests per minute.

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const API_BASE: &str = "https://api.modrinth.com/v2";
pub const API_HOST: &str = "api.modrinth.com";
pub const CDN_HOST: &str = "cdn.modrinth.com";

/// Requests per second the client allows itself (Modrinth's limit is 5/s sustained).
pub const REQUESTS_PER_SECOND: u32 = 5;
/// Requests that may go out back to back before spacing kicks in.
pub const BURST: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    /// JSON body for `Post`.
    pub body: Option<Vec<u8>>,
    /// The response body may not be longer than this.
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpMeta {
    pub status: u16,
    pub retry_after_secs: Option<u64>,
    pub ratelimit_remaining: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("network error: {0}")]
    Network(String),
    #[error("the server answered with a redirect (HTTP {0}), which is not followed")]
    Redirect(u16),
    #[error("the response is larger than the allowed {0} bytes")]
    TooLarge(u64),
    #[error("could not store the response: {0}")]
    Sink(String),
    #[error("unexpected HTTP status {0}")]
    Status(u16),
}

/// Performs one request and streams the body into `sink`.
///
/// Implementations must not follow redirects (a 3xx is returned as its status
/// or as [`HttpError::Redirect`]) and must stop after `req.max_bytes`.
pub trait Transport: Send + Sync {
    fn execute(&self, req: &HttpRequest, sink: &mut dyn Write) -> Result<HttpMeta, HttpError>;
}

/// Accepts `https://cdn.modrinth.com/<path>` and nothing else: exact host,
/// no credentials, no port, no query, no fragment, plain ASCII.
pub fn validate_cdn_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    host == CDN_HOST
        && !path.is_empty()
        && url.len() <= 2048
        && url.bytes().all(|b| (0x21..0x7f).contains(&b))
        && !url.contains(['\\', '?', '#', '@'])
        && !path.split('/').any(|seg| seg == ".." || seg == ".")
}

// --------------------------------------------------------------- rate limit

/// A generic cell rate limiter: `interval` between requests with room for
/// `burst` of them at once.
#[derive(Debug)]
pub struct RateLimiter {
    interval: Duration,
    burst_window: Duration,
    /// Theoretical arrival time of the next request.
    next: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn new(per_second: u32, burst: u32) -> RateLimiter {
        let per_second = per_second.max(1);
        let interval = Duration::from_secs(1) / per_second;
        RateLimiter {
            interval,
            burst_window: interval * burst.saturating_sub(1),
            next: Mutex::new(None),
        }
    }

    /// Reserves a slot at `now` and returns how long the caller must wait.
    pub fn reserve(&self, now: Instant) -> Duration {
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let tat = guard.map_or(now, |t| t.max(now));
        *guard = Some(tat + self.interval);
        let allowed_at = tat.checked_sub(self.burst_window).unwrap_or(now);
        allowed_at.saturating_duration_since(now)
    }
}

// ------------------------------------------------------------------- ureq

/// The real transport.
#[derive(Debug)]
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    /// HTTPS only, no redirects, no proxy, identifying `User-Agent`.
    pub fn new(user_agent: &str) -> UreqTransport {
        Self::build(user_agent, true)
    }

    fn build(user_agent: &str, https_only: bool) -> UreqTransport {
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
        UreqTransport {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// Plain-HTTP variant for tests against a loopback server.
    #[cfg(test)]
    pub fn for_loopback_tests() -> UreqTransport {
        Self::build("craft-a-server-test/0", false)
    }
}

/// Total time allowed for one request: small for API calls, long for jars.
fn total_timeout(max_bytes: u64) -> Duration {
    if max_bytes > 8 * 1024 * 1024 {
        Duration::from_secs(900)
    } else {
        Duration::from_secs(30)
    }
}

fn network(e: &ureq::Error) -> HttpError {
    HttpError::Network(e.to_string())
}

impl Transport for UreqTransport {
    fn execute(&self, req: &HttpRequest, sink: &mut dyn Write) -> Result<HttpMeta, HttpError> {
        let timeout = Some(total_timeout(req.max_bytes));
        let result = match req.method {
            Method::Get => self
                .agent
                .get(&req.url)
                .config()
                .timeout_global(timeout)
                .build()
                .call(),
            Method::Post => self
                .agent
                .post(&req.url)
                .config()
                .timeout_global(timeout)
                .build()
                .header("Content-Type", "application/json")
                .send(req.body.as_deref().unwrap_or_default()),
        };
        let mut response = result.map_err(|e| network(&e))?;
        let status = response.status().as_u16();
        let header_u64 = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let meta = HttpMeta {
            status,
            retry_after_secs: header_u64("retry-after"),
            ratelimit_remaining: header_u64("x-ratelimit-remaining")
                .and_then(|v| u32::try_from(v).ok()),
        };
        if (300..400).contains(&status) {
            return Err(HttpError::Redirect(status));
        }
        if let Some(len) = header_u64("content-length")
            && len > req.max_bytes
        {
            return Err(HttpError::TooLarge(req.max_bytes));
        }
        copy_bounded(&mut response.body_mut().as_reader(), sink, req.max_bytes)?;
        Ok(meta)
    }
}

/// Copies at most `max` bytes; a longer body is an error, not a truncation.
fn copy_bounded(reader: &mut dyn Read, sink: &mut dyn Write, max: u64) -> Result<u64, HttpError> {
    let mut buf = [0_u8; 32 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| HttpError::Network(e.to_string()))?;
        if n == 0 {
            return Ok(total);
        }
        total = total.saturating_add(n as u64);
        if total > max {
            return Err(HttpError::TooLarge(max));
        }
        let chunk = buf.get(..n).unwrap_or_default();
        sink.write_all(chunk)
            .map_err(|e| HttpError::Sink(e.to_string()))?;
    }
}

/// A sink that errors once more than `max` bytes arrive. The client wraps
/// every transport sink in one, so the cap holds even for a transport that
/// forgets it.
#[derive(Debug)]
pub struct CappedVec {
    pub bytes: Vec<u8>,
    max: u64,
}

impl CappedVec {
    pub fn new(max: u64) -> CappedVec {
        CappedVec {
            bytes: Vec::new(),
            max,
        }
    }
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

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn only_the_modrinth_cdn_over_https_is_a_download_url() {
        assert!(validate_cdn_url(
            "https://cdn.modrinth.com/data/P7dR8mSH/versions/EQKgpeKu/fabric-api-0.165.0%2B26.3.jar"
        ));
        for bad in [
            "http://cdn.modrinth.com/data/x/y.jar",
            "https://cdn.modrinth.com.evil.com/data/x.jar",
            "https://evil.com/cdn.modrinth.com/x.jar",
            "https://cdn.modrinth.com@evil.com/x.jar",
            "https://user:pw@cdn.modrinth.com/x.jar",
            "https://cdn.modrinth.com:8443/x.jar",
            "https://cdn.modrinth.com/",
            "https://cdn.modrinth.com",
            "https://api.modrinth.com/v2/project/x",
            "https://cdn.modrinth.com/data/../../etc/passwd",
            "https://cdn.modrinth.com/x.jar?redirect=http://evil",
            "https://cdn.modrinth.com/x.jar#frag",
            "https://cdn.modrinth.com\\@evil.com/x.jar",
            "https://cdn.modrinth.com/a b.jar",
            "https://cdn.modrinth.com/\u{e9}.jar",
            "ftp://cdn.modrinth.com/x.jar",
            "//cdn.modrinth.com/x.jar",
            "",
        ] {
            assert!(!validate_cdn_url(bad), "{bad}");
        }
    }

    #[test]
    fn limiter_allows_a_burst_then_spaces_requests() {
        let l = RateLimiter::new(5, 5);
        let t0 = Instant::now();
        // The first five go out at once.
        for _ in 0..5 {
            assert_eq!(l.reserve(t0), Duration::ZERO);
        }
        // Then one per 200 ms.
        assert_eq!(l.reserve(t0), Duration::from_millis(200));
        assert_eq!(l.reserve(t0), Duration::from_millis(400));
        // After a long pause the burst is available again.
        let later = t0 + Duration::from_secs(60);
        assert_eq!(l.reserve(later), Duration::ZERO);
    }

    #[test]
    fn sustained_rate_stays_under_the_api_limit() {
        let l = RateLimiter::new(REQUESTS_PER_SECOND, BURST);
        let t0 = Instant::now();
        let mut waited = Duration::ZERO;
        for _ in 0..100 {
            waited = waited.max(l.reserve(t0));
        }
        // 100 requests need at least ~19 seconds: under 300 per minute.
        assert!(waited >= Duration::from_secs(19), "{waited:?}");
    }

    #[test]
    fn capped_sink_refuses_more_than_its_cap() {
        let mut s = CappedVec::new(4);
        assert!(s.write_all(b"abcd").is_ok());
        assert!(s.write_all(b"e").is_err());
    }

    /// Serves one canned response on a loopback port; returns the port.
    fn serve_once(response: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").map(|l| (l.local_addr(), l));
        let (addr, listener) = match listener {
            Ok((Ok(addr), l)) => (addr, l),
            _ => return 0,
        };
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0_u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
            }
        });
        addr.port()
    }

    fn get(port: u16, max: u64) -> (Result<HttpMeta, HttpError>, Vec<u8>) {
        let t = UreqTransport::for_loopback_tests();
        let mut sink = Vec::new();
        let r = t.execute(
            &HttpRequest {
                method: Method::Get,
                url: format!("http://127.0.0.1:{port}/x"),
                body: None,
                max_bytes: max,
            },
            &mut sink,
        );
        (r, sink)
    }

    #[test]
    fn real_transport_returns_the_body() {
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nRetry-After: 7\r\nConnection: close\r\n\r\nhello"
                .to_vec(),
        );
        let (r, body) = get(port, 100);
        assert_eq!(body, b"hello");
        assert_eq!(
            r.map(|m| (m.status, m.retry_after_secs)).ok(),
            Some((200, Some(7)))
        );
    }

    #[test]
    fn real_transport_does_not_follow_redirects() {
        let port = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/evil\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
        );
        let (r, body) = get(port, 100);
        assert!(matches!(r, Err(HttpError::Redirect(302))), "{r:?}");
        assert!(body.is_empty());
    }

    #[test]
    fn real_transport_refuses_an_announced_oversize_body() {
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nabc".to_vec(),
        );
        let (r, body) = get(port, 10);
        assert!(matches!(r, Err(HttpError::TooLarge(10))), "{r:?}");
        assert!(body.is_empty());
    }

    #[test]
    fn real_transport_stops_an_unannounced_oversize_stream() {
        // No Content-Length: the body runs until the connection closes.
        let mut resp = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        resp.extend(std::iter::repeat_n(b'a', 5000));
        let port = serve_once(resp);
        let (r, body) = get(port, 100);
        assert!(matches!(r, Err(HttpError::TooLarge(100))), "{r:?}");
        assert!(body.len() <= 100);
    }

    #[test]
    fn real_transport_is_https_only_in_production_mode() {
        let t = UreqTransport::new("craft-a-server-test/0");
        let mut sink = Vec::new();
        let r = t.execute(
            &HttpRequest {
                method: Method::Get,
                url: "http://127.0.0.1:9/x".into(),
                body: None,
                max_bytes: 10,
            },
            &mut sink,
        );
        assert!(matches!(r, Err(HttpError::Network(_))), "{r:?}");
    }
}
