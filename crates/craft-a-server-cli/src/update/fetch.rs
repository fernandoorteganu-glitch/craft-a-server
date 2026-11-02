//! The only network code in the CLI: bounded HTTPS GETs for `update`.
//!
//! Redirects are followed by hand, up to [`MAX_REDIRECTS`], and every hop must
//! be `https://`. The host is not restricted: GitHub serves release assets
//! from a CDN host, and integrity does not depend on where the bytes came from
//! because everything is checked against a signed list afterwards.

use std::io::{self, Read, Write};
use std::time::Duration;

pub const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("refusing a non-https address")]
    NotHttps,
    #[error("network error: {0}")]
    Network(String),
    #[error("the response is larger than the allowed {0} bytes")]
    TooLarge(u64),
    #[error("the server answered HTTP {0}")]
    Status(u16),
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("a redirect pointed somewhere that is not an https address")]
    BadRedirect,
}

/// Fetches a URL into memory. Implementations must stop after `max_bytes`.
pub trait Fetch {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError>;
}

/// HTTPS only, no proxy, no automatic redirects, bounded bodies.
#[derive(Debug)]
pub struct UreqFetch {
    agent: ureq::Agent,
    https_only: bool,
}

impl UreqFetch {
    pub fn new() -> UreqFetch {
        Self::build(true)
    }

    fn build(https_only: bool) -> UreqFetch {
        let config = ureq::Agent::config_builder()
            .https_only(https_only)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .proxy(None)
            .user_agent(concat!("craft-a-server-update/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(30)))
            .build();
        UreqFetch {
            agent: ureq::Agent::new_with_config(config),
            https_only,
        }
    }

    /// Plain-HTTP variant for tests against a loopback server.
    #[cfg(test)]
    pub fn for_loopback_tests() -> UreqFetch {
        Self::build(false)
    }

    fn scheme_ok(&self, url: &str) -> bool {
        url.starts_with("https://") || (!self.https_only && url.starts_with("http://"))
    }
}

impl Default for UreqFetch {
    fn default() -> Self {
        Self::new()
    }
}

fn total_timeout(max_bytes: u64) -> Duration {
    if max_bytes > 1024 * 1024 {
        Duration::from_secs(900)
    } else {
        Duration::from_secs(30)
    }
}

impl Fetch for UreqFetch {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        let mut current = url.to_owned();
        for _ in 0..=MAX_REDIRECTS {
            if !self.scheme_ok(&current) {
                return Err(FetchError::NotHttps);
            }
            let mut response = self
                .agent
                .get(&current)
                .config()
                .timeout_global(Some(total_timeout(max_bytes)))
                .build()
                .call()
                .map_err(|e| FetchError::Network(e.to_string()))?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let target = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
                    .ok_or(FetchError::BadRedirect)?;
                if !self.scheme_ok(&target) {
                    return Err(FetchError::BadRedirect);
                }
                current = target;
                continue;
            }
            if status != 200 {
                return Err(FetchError::Status(status));
            }
            let announced = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            if announced.is_some_and(|len| len > max_bytes) {
                return Err(FetchError::TooLarge(max_bytes));
            }
            let mut body = Vec::new();
            copy_bounded(&mut response.body_mut().as_reader(), &mut body, max_bytes)?;
            return Ok(body);
        }
        Err(FetchError::TooManyRedirects)
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
            .map_err(|e: io::Error| FetchError::Network(e.to_string()))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    /// Serves canned raw HTTP answers, one per connection, in order.
    fn serve(answers: Vec<String>) -> Result<String, io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        std::thread::spawn(move || {
            for answer in answers {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        Ok(format!("http://{addr}"))
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect(to: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn production_client_refuses_plain_http() {
        let f = UreqFetch::new();
        assert_eq!(f.get("http://example.com/x", 10), Err(FetchError::NotHttps));
        assert_eq!(f.get("ftp://example.com/x", 10), Err(FetchError::NotHttps));
        assert_eq!(f.get("file:///etc/passwd", 10), Err(FetchError::NotHttps));
    }

    #[test]
    fn reads_a_body() -> TestResult {
        let base = serve(vec![ok("hello")])?;
        let got = UreqFetch::for_loopback_tests().get(&format!("{base}/a"), 100)?;
        assert_eq!(got, b"hello");
        Ok(())
    }

    #[test]
    fn follows_redirects_between_allowed_schemes() -> TestResult {
        let end = serve(vec![ok("final")])?;
        let start = serve(vec![redirect(&format!("{end}/z"))])?;
        let got = UreqFetch::for_loopback_tests().get(&format!("{start}/a"), 100)?;
        assert_eq!(got, b"final");
        Ok(())
    }

    #[test]
    fn a_redirect_to_a_non_http_scheme_is_refused() -> TestResult {
        let start = serve(vec![redirect("file:///etc/passwd")])?;
        assert_eq!(
            UreqFetch::for_loopback_tests().get(&format!("{start}/a"), 100),
            Err(FetchError::BadRedirect)
        );
        Ok(())
    }

    #[test]
    fn production_client_would_refuse_a_redirect_downgrade_to_http() {
        let f = UreqFetch::new();
        assert!(!f.scheme_ok("http://example.com"));
        assert!(f.scheme_ok("https://example.com"));
    }

    #[test]
    fn redirect_loops_stop() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let _ = stream.write_all(redirect(&format!("http://{addr}/again")).as_bytes());
            }
        });
        assert_eq!(
            UreqFetch::for_loopback_tests().get(&format!("http://{addr}/a"), 100),
            Err(FetchError::TooManyRedirects)
        );
        Ok(())
    }

    #[test]
    fn oversize_bodies_are_errors_not_truncations() -> TestResult {
        let base = serve(vec![ok("0123456789")])?;
        assert_eq!(
            UreqFetch::for_loopback_tests().get(&format!("{base}/a"), 5),
            Err(FetchError::TooLarge(5))
        );
        // Without a Content-Length the streaming cap must still hold.
        let chunked = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n0123456789".to_owned();
        let base = serve(vec![chunked])?;
        assert_eq!(
            UreqFetch::for_loopback_tests().get(&format!("{base}/a"), 5),
            Err(FetchError::TooLarge(5))
        );
        Ok(())
    }

    #[test]
    fn non_200_answers_are_errors() -> TestResult {
        let base = serve(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ])?;
        assert_eq!(
            UreqFetch::for_loopback_tests().get(&format!("{base}/a"), 5),
            Err(FetchError::Status(404))
        );
        Ok(())
    }
}
