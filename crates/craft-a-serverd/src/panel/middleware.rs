//! The request pipeline. See the module docs of `panel` for the order.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::auth::{self, AuthSession, CSRF_HEADER, TOUCH_INTERVAL_SECS, clear_cookie, ct_eq};
use super::error::{ErrorMarker, PanelError, Rendered};
use super::net::{host_allowed, origin_ok, peer_allowed, unmap};
use super::pages;
use super::routes::Access;
use super::{GuardState, MAX_BODY_BYTES, PanelCore, REQUEST_TIMEOUT};
use crate::db::hash_secret;

pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
const PERMISSIONS_POLICY: &str = "accelerometer=(), ambient-light-sensor=(), autoplay=(), battery=(), \
camera=(), display-capture=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), \
midi=(), payment=(), usb=(), interest-cohort=()";
const MAX_COOKIE_HEADER: usize = 4096;
const MAX_HOST_HEADER: usize = 255;
const MAX_ORIGIN_HEADER: usize = 512;

/// The peer address, IPv4-mapped IPv6 already turned into IPv4.
#[derive(Debug, Clone, Copy)]
pub struct PeerIp(pub IpAddr);

/// The validated `Host` header value.
#[derive(Debug, Clone)]
struct ValidHost(String);

/// Public id of the session that made the request, for the access log.
#[derive(Debug, Clone)]
struct LogSession(String);

/// Does the caller want machine-readable output?
pub fn wants_json(headers: &HeaderMap, path: &str) -> bool {
    path.starts_with("/api/")
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("application/json"))
}

/// Is this an htmx request?
pub fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

fn is_state_changing(m: &Method) -> bool {
    matches!(
        *m,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn bare(status: StatusCode) -> Response {
    let mut res = Response::new(Body::empty());
    *res.status_mut() = status;
    res
}

/// Outermost: security headers on every response, whatever produced it.
pub(super) async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, name: &'static str, value: &'static str| {
        h.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    };
    set(h, "content-security-policy", CSP);
    set(h, "x-content-type-options", "nosniff");
    // `same-origin`, not `no-referrer`: with no-referrer browsers send `Origin: null` on
    // form posts, which the Origin check (rightly) rejects, so nothing could be submitted.
    // Nothing is ever sent cross-origin either way.
    set(h, "referrer-policy", "same-origin");
    set(h, "x-frame-options", "DENY");
    set(h, "cross-origin-opener-policy", "same-origin");
    set(h, "cross-origin-resource-policy", "same-origin");
    set(h, "permissions-policy", PERMISSIONS_POLICY);
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}

/// Access log at debug level: request id, method, path (no query), status and
/// the public session id. Never headers, cookies or bodies.
pub(super) async fn trace(
    State(core): State<Arc<PanelCore>>,
    req: Request,
    next: Next,
) -> Response {
    let id = format!("{:08x}", core.request_seq.fetch_add(1, Ordering::Relaxed));
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let started = Instant::now();
    let mut res = next.run(req).await;
    let session = res.extensions().get::<LogSession>().map(|s| s.0.clone());
    tracing::debug!(
        request_id = %id,
        %method,
        path = %path,
        status = res.status().as_u16(),
        session = session.as_deref().unwrap_or("-"),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "panel request"
    );
    if let Ok(v) = HeaderValue::from_str(&id) {
        res.headers_mut()
            .insert(HeaderName::from_static("x-request-id"), v);
    }
    res
}

/// Refuses anyone who is not on the local network, with a bare 403.
pub(super) async fn peer_filter(mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    match peer {
        Some(ip) if peer_allowed(ip) => {
            req.extensions_mut().insert(PeerIp(unmap(ip)));
            next.run(req).await
        }
        Some(ip) => {
            tracing::debug!(peer = %ip, "refused a connection from outside the local network");
            bare(StatusCode::FORBIDDEN)
        }
        None => bare(StatusCode::FORBIDDEN),
    }
}

/// Turns every error response into the friendly page or JSON object.
pub(super) async fn shape_errors(req: Request, next: Next) -> Response {
    let json = wants_json(req.headers(), req.uri().path());
    let res = next.run(req).await;
    let status = res.status();
    if !(status.is_client_error() || status.is_server_error())
        || res.extensions().get::<Rendered>().is_some()
    {
        return res;
    }
    let err = res
        .extensions()
        .get::<ErrorMarker>()
        .map_or_else(|| PanelError::from_status(status), |m| m.0.clone());
    let mut out = pages::error_response(&err, json);
    // Keep protocol headers (Retry-After, Allow, Set-Cookie, HX-*); the body
    // headers belong to the replaced body.
    for (name, value) in res.headers() {
        if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
            out.headers_mut().append(name.clone(), value.clone());
        }
    }
    out
}

fn single_header<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
    max: usize,
) -> Result<Option<&'a str>, ()> {
    let mut all = headers.get_all(name).iter();
    let Some(first) = all.next() else {
        return Ok(None);
    };
    if all.next().is_some() || first.len() > max {
        return Err(());
    }
    first.to_str().map(Some).map_err(|_| ())
}

/// DNS rebinding defence: only known `Host` values pass.
pub(super) async fn host_check(
    State(core): State<Arc<PanelCore>>,
    mut req: Request,
    next: Next,
) -> Response {
    let host = match single_header(req.headers(), &header::HOST, MAX_HOST_HEADER) {
        Ok(Some(h)) => h.to_owned(),
        _ => return PanelError::HostRejected.into_response(),
    };
    if !host_allowed(&host, &core.cfg.panel_hostname) {
        return PanelError::HostRejected.into_response();
    }
    // An absolute-form request target must agree with the Host header.
    if let Some(auth) = req.uri().authority()
        && !auth.as_str().eq_ignore_ascii_case(&host)
    {
        return PanelError::HostRejected.into_response();
    }
    req.extensions_mut().insert(ValidHost(host));
    next.run(req).await
}

/// Body size, concurrency and time limits.
pub(super) async fn limits(
    State(core): State<Arc<PanelCore>>,
    req: Request,
    next: Next,
) -> Response {
    let too_big = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_BODY_BYTES as u64);
    if too_big {
        return PanelError::PayloadTooLarge.into_response();
    }
    let Ok(_permit) = Arc::clone(&core.inflight).try_acquire_owned() else {
        return PanelError::Busy.into_response();
    };
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(req)).await {
        Ok(res) => res,
        Err(_) => PanelError::Timeout.into_response(),
    }
}

fn unauthenticated(req: &Request, stale_cookie: bool) -> Response {
    let hx = is_htmx(req.headers());
    let browser_nav = matches!(*req.method(), Method::GET | Method::HEAD)
        && !hx
        && !wants_json(req.headers(), req.uri().path());
    let mut res = if browser_nav {
        let mut r = bare(StatusCode::SEE_OTHER);
        r.headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/pair"));
        r
    } else {
        let mut r = PanelError::Unauthorized.into_response();
        if hx {
            r.headers_mut()
                .insert("hx-redirect", HeaderValue::from_static("/pair"));
        }
        r
    };
    if stale_cookie && let Ok(v) = HeaderValue::from_str(&clear_cookie()) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
    res
}

/// Route layer: Origin, session and CSRF, according to the route's [`Access`].
/// Routes missing from the route table are treated as `Access::Session`.
pub(super) async fn guard(State(g): State<GuardState>, mut req: Request, next: Next) -> Response {
    let access = req
        .extensions()
        .get::<MatchedPath>()
        .and_then(|m| g.access.get(m.as_str()))
        .copied()
        .unwrap_or(Access::Session);
    let state_changing = is_state_changing(req.method());

    if state_changing && !origin_matches(&req) {
        return PanelError::OriginRejected.into_response();
    }
    if access != Access::Session {
        return next.run(req).await;
    }

    // Authenticate.
    let cookie_token = token_from_request(&req);
    let (session, token) = match resolve_session(&g.core, cookie_token).await {
        Ok(Some(found)) => found,
        Ok(None) => return unauthenticated(&req, has_cookie(&req)),
        Err(e) => return e.into_response(),
    };

    // CSRF for every state-changing request on a session route.
    if state_changing && let Err(e) = check_csrf(&mut req, &session.csrf_token).await {
        return e.into_response();
    }

    // Sliding expiry: the row is rewritten at most every few minutes.
    let now = g.core.now();
    let ttl = g.core.cfg.session_ttl_secs;
    let mut refresh = None;
    if now - session.last_seen >= TOUCH_INTERVAL_SECS {
        let hash = hash_secret(&token);
        let new_expiry = now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX / 2));
        let touched = g
            .core
            .blocking(move |db| Ok(db.touch_session(&hash, now, new_expiry)?))
            .await;
        match touched {
            Ok(()) => refresh = Some(auth::session_cookie(&token, ttl)),
            Err(e) => return e.into_response(),
        }
    }

    let log_id = session.auth.id.clone();
    req.extensions_mut().insert(session.auth);
    let mut res = next.run(req).await;
    if let Some(cookie) = refresh
        && !res.headers().contains_key(header::SET_COOKIE)
        && let Ok(v) = HeaderValue::from_str(&cookie)
    {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
    res.extensions_mut().insert(LogSession(log_id));
    res
}

fn has_cookie(req: &Request) -> bool {
    req.headers().contains_key(header::COOKIE)
}

fn origin_matches(req: &Request) -> bool {
    let Some(ValidHost(host)) = req.extensions().get::<ValidHost>() else {
        return false;
    };
    let h = req.headers();
    let origin = match single_header(h, &header::ORIGIN, MAX_ORIGIN_HEADER) {
        Ok(v) => v,
        Err(()) => return false,
    };
    let referer = match single_header(h, &header::REFERER, 2048) {
        Ok(v) => v,
        Err(()) => return false,
    };
    origin_ok(origin, referer, host)
}

struct Resolved {
    auth: AuthSession,
    csrf_token: String,
    last_seen: i64,
}

/// The well-formed session token in the request's cookies, if any.
fn token_from_request(req: &Request) -> Option<String> {
    let mut joined = String::new();
    for v in req.headers().get_all(header::COOKIE) {
        let Ok(text) = v.to_str() else { continue };
        if joined.len() + text.len() > MAX_COOKIE_HEADER {
            return None;
        }
        if !joined.is_empty() {
            joined.push_str("; ");
        }
        joined.push_str(text);
    }
    auth::token_from_cookie_header(&joined).map(str::to_owned)
}

async fn resolve_session(
    core: &Arc<PanelCore>,
    token: Option<String>,
) -> Result<Option<(Resolved, String)>, PanelError> {
    let Some(token) = token else {
        return Ok(None);
    };
    let hash = hash_secret(&token);
    let now = core.now();
    let row = core
        .blocking(move |db| Ok(db.find_session(&hash, now)?))
        .await?;
    Ok(row.map(|r| {
        (
            Resolved {
                auth: AuthSession {
                    id: r.id,
                    device_name: r.device_name,
                    csrf_token: r.csrf_token.clone(),
                },
                csrf_token: r.csrf_token,
                last_seen: r.last_seen_unix,
            },
            token,
        )
    }))
}

/// Verifies the CSRF token from `X-CSRF-Token` or, for urlencoded forms, the
/// `csrf_token` field (the body is buffered and handed back to the handler).
async fn check_csrf(req: &mut Request, expected: &str) -> Result<(), PanelError> {
    if let Some(v) = req.headers().get(CSRF_HEADER) {
        let supplied = v.to_str().map_err(|_| PanelError::CsrfFailed)?;
        return if ct_eq(supplied, expected) {
            Ok(())
        } else {
            Err(PanelError::CsrfFailed)
        };
    }
    let is_form = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("application/x-www-form-urlencoded")
        });
    if !is_form {
        return Err(PanelError::CsrfFailed);
    }
    let body = std::mem::replace(req.body_mut(), Body::empty());
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| PanelError::PayloadTooLarge)?;
    let supplied = auth::csrf_from_form(&bytes);
    *req.body_mut() = Body::from(bytes);
    match supplied {
        Some(s) if ct_eq(&s, expected) => Ok(()),
        _ => Err(PanelError::CsrfFailed),
    }
}
