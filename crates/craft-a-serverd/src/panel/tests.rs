// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! End-to-end tests of the panel's security pipeline, driven through the real
//! router with `tower::ServiceExt::oneshot` and an injected `ConnectInfo`
//! (so the peer filter is exercised). Platform neutral: an in-memory database,
//! a fake container runtime and an injectable clock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use axum::http::{Method, Request, StatusCode};
use axum::routing::get;
use craft_a_server_core::config::Config;
use craft_a_server_core::mc_protocol::ServerStatus;
use tower::ServiceExt;

use super::auth::{self, display_code, issue_pairing_code};
use super::middleware::CSP;
use super::routes::{self, Access};
use super::{PanelCore, PanelState, router};
use crate::db::{Db, hash_secret};
use crate::lifecycle::{DiskUsage, Env, Service, StatusProbe};
use crate::mods::testutil::FakeTransport;
use crate::runtime::FakeRuntime;
use crate::tunnel::{
    FakeFetch, Platform, Release, Timings, TunnelDirs, TunnelManager, TunnelSettings,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const HOST: &str = "craft-a-server.local:8080";
const ORIGIN: &str = "http://craft-a-server.local:8080";
const T0: i64 = 1_700_000_000;
const SESSION_TTL: i64 = 2_592_000;
const GOOD_PEER: &str = "192.168.1.20:40000";

/// A status probe that says "not up" until a test flips it.
#[derive(Default)]
struct NoProbe {
    up: AtomicBool,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl NoProbe {
    fn set_up(&self, up: bool) {
        self.up.store(up, Ordering::SeqCst);
    }
}

impl StatusProbe for NoProbe {
    async fn status(&self, _port: u16) -> Option<ServerStatus> {
        self.up.load(Ordering::SeqCst).then(|| ServerStatus {
            version_name: "26.3".into(),
            protocol: 777,
            players_online: 0,
            players_max: 20,
        })
    }
}

type Svc = Service<FakeRuntime, NoProbe>;

struct Harness {
    svc: Arc<Svc>,
    clock: Arc<AtomicI64>,
    app: Router,
    next_peer: AtomicU16,
    /// The fake Modrinth behind the service.
    net: FakeTransport,
}

fn harness() -> Harness {
    harness_with(|_| {})
}

fn harness_with(tweak: impl FnOnce(&mut Config)) -> Harness {
    harness_full(tweak, Some(LAN_IP))
}

const LAN_IP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 168, 1, 50);

fn harness_full(tweak: impl FnOnce(&mut Config), lan_ip: Option<std::net::Ipv4Addr>) -> Harness {
    harness_build(tweak, lan_ip, true)
}

/// A panel over a service that has no tunnel manager.
fn harness_without_tunnel() -> Harness {
    harness_build(|_| {}, Some(LAN_IP), false)
}

fn harness_build(
    tweak: impl FnOnce(&mut Config),
    lan_ip: Option<std::net::Ipv4Addr>,
    with_tunnel: bool,
) -> Harness {
    let mut cfg = Config {
        data_dir: std::env::temp_dir(),
        ..Config::default()
    };
    tweak(&mut cfg);
    let clock = Arc::new(AtomicI64::new(T0));
    let c = Arc::clone(&clock);
    let env = Env {
        total_ram_bytes: 8 << 30,
        uid: 1000,
        gid: 1000,
        now_unix: Arc::new(move || c.load(Ordering::SeqCst)),
        disk: Arc::new(|_| {
            Ok(DiskUsage {
                free_bytes: 90 << 30,
                total_bytes: 100 << 30,
            })
        }),
        poll_interval: std::time::Duration::from_millis(10),
    };
    let net = FakeTransport::new();
    let db = Db::open_in_memory().expect("db");
    // Online play with a fake network: the panel tests never run the
    // supervisor, they only look at the stored state and the buttons.
    let tunnel = Arc::new(TunnelManager::new(
        TunnelSettings {
            dirs: TunnelDirs::new(&cfg.tunnel_dir()),
            mc_port: cfg.mc_port,
            release: Release::pinned(),
            platform: Some(Platform::LinuxAmd64),
            rundata_url: "https://api.playit.invalid/agents/rundata".to_owned(),
            timings: Timings::default(),
        },
        db.clone(),
        Arc::new(FakeFetch::default()),
        Arc::clone(&env.now_unix),
    ));
    let service = Service::new(
        Arc::new(cfg),
        db,
        Arc::new(FakeRuntime::new()),
        Arc::new(NoProbe::default()),
        env,
    )
    .with_modrinth(net.client());
    let svc = Arc::new(if with_tunnel {
        service.with_tunnel(tunnel)
    } else {
        service
    });
    let app = router(PanelState::new(Arc::clone(&svc)).with_lan_ip(lan_ip));
    Harness {
        svc,
        clock,
        app,
        next_peer: AtomicU16::new(100),
        net,
    }
}

/// Request builder with sane defaults: good Host, LAN peer and, for
/// state-changing methods, a matching Origin.
struct Rb {
    method: Method,
    path: String,
    host: Option<Vec<u8>>,
    peer: Option<SocketAddr>,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
    auto_origin: bool,
}

impl Rb {
    fn new(method: Method, path: &str) -> Rb {
        Rb {
            method,
            path: path.to_owned(),
            host: Some(HOST.as_bytes().to_vec()),
            peer: Some(GOOD_PEER.parse().expect("peer")),
            headers: Vec::new(),
            body: Vec::new(),
            auto_origin: true,
        }
    }
    fn get(path: &str) -> Rb {
        Rb::new(Method::GET, path)
    }
    fn post(path: &str) -> Rb {
        Rb::new(Method::POST, path)
    }
    fn delete(path: &str) -> Rb {
        Rb::new(Method::DELETE, path)
    }
    fn host(mut self, h: &str) -> Rb {
        self.host = Some(h.as_bytes().to_vec());
        self
    }
    fn no_host(mut self) -> Rb {
        self.host = None;
        self
    }
    fn peer(mut self, addr: &str) -> Rb {
        self.peer = Some(addr.parse().expect("peer address"));
        self
    }
    fn no_peer(mut self) -> Rb {
        self.peer = None;
        self
    }
    fn hdr(mut self, k: &str, v: &str) -> Rb {
        self.headers.push((k.to_owned(), v.as_bytes().to_vec()));
        self
    }
    fn raw_hdr(mut self, k: &str, v: &[u8]) -> Rb {
        self.headers.push((k.to_owned(), v.to_vec()));
        self
    }
    fn origin(self, v: &str) -> Rb {
        self.no_origin().hdr("origin", v)
    }
    fn no_origin(mut self) -> Rb {
        self.auto_origin = false;
        self
    }
    fn cookie(self, token: &str) -> Rb {
        self.hdr("cookie", &format!("craft_session={token}"))
    }
    fn csrf(self, token: &str) -> Rb {
        self.hdr("x-csrf-token", token)
    }
    fn body(mut self, b: &[u8]) -> Rb {
        self.body = b.to_vec();
        self
    }
    fn json(self, s: &str) -> Rb {
        self.hdr("content-type", "application/json")
            .body(s.as_bytes())
    }
    fn form(self, s: &str) -> Rb {
        self.hdr("content-type", "application/x-www-form-urlencoded")
            .body(s.as_bytes())
    }
    fn build(self) -> Request<Body> {
        let mut b = Request::builder()
            .method(self.method.clone())
            .uri(&self.path);
        let mut has_origin = false;
        if let Some(h) = &self.host {
            b = b.header("host", HeaderValue::from_bytes(h).expect("host bytes"));
        }
        for (k, v) in &self.headers {
            has_origin |= k == "origin" || k == "referer";
            b = b.header(
                HeaderName::from_bytes(k.as_bytes()).expect("header name"),
                HeaderValue::from_bytes(v).expect("header value"),
            );
        }
        let changing = matches!(
            self.method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        );
        if changing && self.auto_origin && !has_origin {
            b = b.header("origin", ORIGIN);
        }
        let mut req = b.body(Body::from(self.body)).expect("request");
        if let Some(peer) = self.peer {
            req.extensions_mut().insert(ConnectInfo(peer));
        }
        req
    }
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect()
    }
    fn err_field(&self, field: &str) -> String {
        self.json()
            .get("error")
            .and_then(|e| e.get(field))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    }
    fn err_code(&self) -> String {
        self.err_field("code")
    }
    fn err_message(&self) -> String {
        self.err_field("message")
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A signed-in phone.
struct Sess {
    token: String,
    csrf: String,
    id: String,
}

impl Harness {
    async fn send(&self, rb: Rb) -> Resp {
        self.send_req(rb.build()).await
    }

    async fn send_req(&self, req: Request<Body>) -> Resp {
        let res = self.app.clone().oneshot(req).await.expect("infallible");
        let (parts, body) = res.into_parts();
        let body = axum::body::to_bytes(body, 1 << 20).await.expect("body");
        Resp {
            status: parts.status,
            headers: parts.headers,
            body: body.to_vec(),
        }
    }

    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }

    fn db(&self) -> &Db {
        self.svc.db()
    }

    fn fresh_code(&self) -> String {
        issue_pairing_code(self.db(), self.now(), 600)
            .expect("issue code")
            .code
    }

    /// Pairs a new phone from its own LAN address (so the per-IP limiter is
    /// not what these tests exercise).
    async fn pair(&self, device: &str) -> Sess {
        let n = self.next_peer.fetch_add(1, Ordering::SeqCst);
        let code = display_code(&self.fresh_code());
        let res = self
            .send(
                Rb::post("/api/pair")
                    .peer(&format!("192.168.1.{}:5000", n % 250))
                    .json(&format!(r#"{{"code":"{code}","device_name":"{device}"}}"#)),
            )
            .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let cookie = res.set_cookies().into_iter().next().expect("cookie");
        let token = cookie
            .strip_prefix("craft_session=")
            .and_then(|r| r.split(';').next())
            .expect("token")
            .to_owned();
        let csrf = res
            .json()
            .get("csrf_token")
            .and_then(|v| v.as_str())
            .expect("csrf")
            .to_owned();
        let list = self
            .send(Rb::get("/api/sessions").cookie(&token))
            .await
            .json();
        let id = list
            .as_array()
            .and_then(|a| a.iter().find(|s| s["current"] == true))
            .and_then(|s| s["id"].as_str())
            .expect("current session")
            .to_owned();
        Sess { token, csrf, id }
    }
}

fn assert_security_headers(res: &Resp, what: &str) {
    assert_eq!(res.header("content-security-policy"), Some(CSP), "{what}");
    assert_eq!(
        res.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(res.header("referrer-policy"), Some("same-origin"), "{what}");
    assert_eq!(res.header("x-frame-options"), Some("DENY"), "{what}");
    assert_eq!(
        res.header("cross-origin-opener-policy"),
        Some("same-origin"),
        "{what}"
    );
    assert_eq!(
        res.header("cross-origin-resource-policy"),
        Some("same-origin"),
        "{what}"
    );
    let pp = res.header("permissions-policy").unwrap_or_default();
    for feature in [
        "camera=()",
        "microphone=()",
        "geolocation=()",
        "usb=()",
        "payment=()",
    ] {
        assert!(pp.contains(feature), "{what}: {pp}");
    }
    assert!(res.header("cache-control").is_some(), "{what}");
}

// ---------------------------------------------------------------- headers

#[tokio::test]
async fn security_headers_are_on_every_kind_of_response() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    let big = vec![b'a'; 40_000];
    let cases: Vec<(&str, StatusCode, Rb)> = vec![
        ("200 page", StatusCode::OK, Rb::get("/pair")),
        ("200 health", StatusCode::OK, Rb::get("/healthz")),
        ("200 static", StatusCode::OK, Rb::get("/static/panel.css")),
        (
            "200 json",
            StatusCode::OK,
            Rb::get("/api/sessions").cookie(&s.token),
        ),
        ("303 redirect", StatusCode::SEE_OTHER, Rb::get("/")),
        (
            "401 api",
            StatusCode::UNAUTHORIZED,
            Rb::get("/api/sessions"),
        ),
        (
            "403 peer",
            StatusCode::FORBIDDEN,
            Rb::get("/pair").peer("8.8.8.8:1"),
        ),
        (
            "403 csrf",
            StatusCode::FORBIDDEN,
            Rb::post("/api/logout").cookie(&s.token),
        ),
        ("404", StatusCode::NOT_FOUND, Rb::get("/nope")),
        (
            "405",
            StatusCode::METHOD_NOT_ALLOWED,
            Rb::new(Method::PUT, "/pair"),
        ),
        (
            "413",
            StatusCode::PAYLOAD_TOO_LARGE,
            Rb::post("/api/pair").json("{}").body(&big),
        ),
        (
            "421 host",
            StatusCode::MISDIRECTED_REQUEST,
            Rb::get("/pair").host("evil.com"),
        ),
    ];
    for (what, want, rb) in cases {
        let res = h.send(rb).await;
        assert_eq!(res.status, want, "{what}");
        assert_security_headers(&res, what);
        if what != "200 static" {
            assert_eq!(res.header("cache-control"), Some("no-store"), "{what}");
        }
    }
    Ok(())
}

#[test]
fn csp_is_strict() {
    for bad in ["unsafe-inline", "unsafe-eval", "*", "http:", "https:"] {
        assert!(!CSP.contains(bad), "{bad}");
    }
    for want in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "img-src 'self' data:",
        "connect-src 'self'",
        "form-action 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
    ] {
        assert!(CSP.contains(want), "{want}");
    }
}

// ----------------------------------------------------------------- network

#[tokio::test]
async fn peer_filter_applies_to_every_request() -> TestResult {
    let h = harness();
    for peer in [
        "8.8.8.8:1000",
        "100.64.1.1:1000",
        "[2001:db8::1]:1000",
        "[2606:4700::1111]:1000",
        "[::ffff:8.8.8.8]:1000",
        "0.0.0.0:1000",
    ] {
        let res = h.send(Rb::get("/healthz").peer(peer)).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{peer}");
        assert!(res.body.is_empty(), "{peer}: no detail in the body");
    }
    for peer in [
        "127.0.0.1:1",
        "10.1.1.1:1",
        "172.20.0.5:1",
        "192.168.9.9:1",
        "169.254.3.3:1",
        "[::1]:1",
        "[fd12::1]:1",
        "[fe80::1]:1",
        "[::ffff:192.168.1.5]:1",
    ] {
        let res = h.send(Rb::get("/healthz").peer(peer)).await;
        assert_eq!(res.status, StatusCode::OK, "{peer}");
    }
    // No connection info at all is refused, not trusted.
    assert_eq!(
        h.send(Rb::get("/healthz").no_peer()).await.status,
        StatusCode::FORBIDDEN
    );
    Ok(())
}

#[tokio::test]
async fn host_header_is_checked_on_every_request() -> TestResult {
    let h = harness();
    for host in [
        "craft-a-server.local",
        "craft-a-server.local:8080",
        "CRAFT-A-SERVER.LOCAL",
        "localhost:8080",
        "192.168.1.5:8080",
        "[fd00::1]:8080",
    ] {
        assert_eq!(
            h.send(Rb::get("/healthz").host(host)).await.status,
            StatusCode::OK,
            "{host}"
        );
    }
    for host in [
        "evil.com",
        "craft-a-server.local.evil.com",
        "craft-a-server.local@evil.com",
        "craft-a-server.local.",
        "user@craft-a-server.local",
        "8.8.8.8",
        "[2001:db8::1]",
        "",
    ] {
        let res = h.send(Rb::get("/healthz").host(host)).await;
        assert_eq!(res.status, StatusCode::MISDIRECTED_REQUEST, "{host:?}");
        assert!(!res.text().contains("evil"), "no echo of the bad value");
    }
    assert_eq!(
        h.send(Rb::get("/healthz").no_host()).await.status,
        StatusCode::MISDIRECTED_REQUEST
    );
    // Duplicate and non-UTF-8 Host headers.
    let dup = h.send(Rb::get("/healthz").hdr("host", "evil.com")).await;
    assert_eq!(dup.status, StatusCode::MISDIRECTED_REQUEST);
    let raw = h
        .send(Rb::get("/healthz").host("x").raw_hdr("host", &[0xff, 0xfe]))
        .await;
    assert_eq!(raw.status, StatusCode::MISDIRECTED_REQUEST);
    // A configured hostname replaces the default one.
    let custom = harness_with(|c| c.panel_hostname = "mc.home".into());
    assert_eq!(
        custom
            .send(Rb::get("/healthz").host("mc.home"))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        custom
            .send(Rb::get("/healthz").host("craft-a-server.local"))
            .await
            .status,
        StatusCode::MISDIRECTED_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn origin_is_required_on_state_changing_requests() -> TestResult {
    let h = harness();
    let body = r#"{"code":"AAAAAAAA"}"#;
    // (label, builder, expect the Origin gate to refuse)
    let referer_ok = format!("{ORIGIN}/pair");
    let cases: Vec<(&str, Rb, bool)> = vec![
        ("matching origin", Rb::post("/api/pair").json(body), false),
        (
            "no origin, no referer",
            Rb::post("/api/pair").no_origin().json(body),
            true,
        ),
        (
            "origin null",
            Rb::post("/api/pair").origin("null").json(body),
            true,
        ),
        (
            "foreign origin",
            Rb::post("/api/pair").origin("http://evil.com").json(body),
            true,
        ),
        (
            "origin with wrong port",
            Rb::post("/api/pair")
                .origin("http://craft-a-server.local:9999")
                .json(body),
            true,
        ),
        (
            "https origin",
            Rb::post("/api/pair")
                .origin("https://craft-a-server.local:8080")
                .json(body),
            true,
        ),
        (
            "good referer when origin absent",
            Rb::post("/api/pair")
                .no_origin()
                .hdr("referer", &referer_ok)
                .json(body),
            false,
        ),
        (
            "bad referer when origin absent",
            Rb::post("/api/pair")
                .no_origin()
                .hdr("referer", "http://evil.com/")
                .json(body),
            true,
        ),
        (
            "bad origin is not rescued by a good referer",
            Rb::post("/api/pair")
                .origin("null")
                .hdr("referer", &referer_ok)
                .json(body),
            true,
        ),
        (
            "duplicate origin headers",
            Rb::post("/api/pair")
                .origin(ORIGIN)
                .hdr("origin", ORIGIN)
                .json(body),
            true,
        ),
        (
            "non-UTF-8 origin",
            Rb::post("/api/pair")
                .no_origin()
                .raw_hdr("origin", &[0xff, 0xfe])
                .json(body),
            true,
        ),
    ];
    for (label, rb, refused) in cases {
        let res = h.send(rb).await;
        if refused {
            assert_eq!(res.status, StatusCode::FORBIDDEN, "{label}");
            assert_eq!(res.err_code(), "origin_rejected", "{label}");
        } else {
            // Past the gate: the code is simply wrong.
            assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}");
            assert_eq!(res.err_code(), "pair_invalid", "{label}");
        }
    }
    // Safe methods need no Origin.
    assert_eq!(
        h.send(Rb::get("/pair").no_origin()).await.status,
        StatusCode::OK
    );
    Ok(())
}

// -------------------------------------------------------------------- CSRF

#[tokio::test]
async fn csrf_token_is_required_and_checked() -> TestResult {
    let h = harness();
    let a = h.pair("Pixel").await;
    let b = h.pair("iPad").await;
    let missing = h.send(Rb::post("/api/logout").cookie(&a.token)).await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);
    assert_eq!(missing.err_code(), "csrf_failed");
    let wrong = h
        .send(Rb::post("/api/logout").cookie(&a.token).csrf("deadbeef"))
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    // Another session's token is wrong too.
    let other = h
        .send(Rb::post("/api/logout").cookie(&a.token).csrf(&b.csrf))
        .await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);
    // Same-length near miss.
    let mut near = a.csrf.clone();
    near.pop();
    near.push(if a.csrf.ends_with('0') { '1' } else { '0' });
    assert_eq!(
        h.send(Rb::post("/api/logout").cookie(&a.token).csrf(&near))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // Form field missing / wrong / right.
    for (form, want) in [
        ("x=1".to_owned(), StatusCode::FORBIDDEN),
        ("csrf_token=nope".to_owned(), StatusCode::FORBIDDEN),
        (format!("csrf_token={}", b.csrf), StatusCode::FORBIDDEN),
    ] {
        let res = h
            .send(Rb::post("/api/logout").cookie(&a.token).form(&form))
            .await;
        assert_eq!(res.status, want, "{form}");
    }
    // A wrong header is final even with a right field.
    let both = h
        .send(
            Rb::post("/api/logout")
                .cookie(&a.token)
                .csrf("nope")
                .form(&format!("csrf_token={}", a.csrf)),
        )
        .await;
    assert_eq!(both.status, StatusCode::FORBIDDEN);
    // JSON bodies cannot carry the token.
    let json = h
        .send(
            Rb::post("/api/logout")
                .cookie(&a.token)
                .json(&format!(r#"{{"csrf_token":"{}"}}"#, a.csrf)),
        )
        .await;
    assert_eq!(json.status, StatusCode::FORBIDDEN);
    // None of the failures logged the user out.
    assert_eq!(
        h.send(Rb::get("/").cookie(&a.token)).await.status,
        StatusCode::OK
    );

    // Right token in the header, and in the form (plain HTML flow).
    let ok = h
        .send(Rb::post("/api/logout").cookie(&b.token).csrf(&b.csrf))
        .await;
    assert_eq!(ok.status, StatusCode::OK);
    let form = h
        .send(
            Rb::post("/api/logout")
                .cookie(&a.token)
                .form(&format!("csrf_token={}", a.csrf)),
        )
        .await;
    assert_eq!(form.status, StatusCode::SEE_OTHER);
    assert_eq!(form.header("location"), Some("/pair"));
    Ok(())
}

#[tokio::test]
async fn csrf_is_checked_centrally_for_every_state_changing_method() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    // Even methods with no handler get the guard first on routes that exist;
    // the checks never depend on handler code.
    for (method, path) in [
        (Method::DELETE, format!("/api/sessions/{}", s.id)),
        (Method::POST, format!("/api/sessions/{}/revoke", s.id)),
        (Method::POST, "/api/logout".to_owned()),
    ] {
        let res = h
            .send(Rb::new(method.clone(), &path).cookie(&s.token))
            .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert_eq!(
        h.send(Rb::get("/").cookie(&s.token)).await.status,
        StatusCode::OK
    );
    Ok(())
}

// ----------------------------------------------------------------- pairing

#[tokio::test]
async fn pairing_sets_an_exact_cookie_and_stores_only_hashes() -> TestResult {
    let h = harness();
    let code = h.fresh_code();
    let res = h
        .send(Rb::post("/api/pair").json(&format!(
            r#"{{"code":"{}"}}"#,
            display_code(&code).to_lowercase()
        )))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    let cookies = res.set_cookies();
    assert_eq!(cookies.len(), 1);
    let cookie = cookies.first().expect("cookie");
    let token = cookie
        .strip_prefix("craft_session=")
        .and_then(|r| r.strip_suffix("; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"))
        .expect("exact cookie attributes");
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!cookie.contains("Secure"));
    // The database has the hash, never the token or the code.
    let rows = h.db().list_sessions(h.now())?;
    let row = rows.first().expect("one session");
    assert_eq!(row.token_hash, hash_secret(token));
    assert_ne!(row.token_hash, token);
    assert_eq!(row.expires_unix, h.now() + SESSION_TTL);
    assert_eq!(row.device_name, "Browser");
    assert_eq!(h.db().count_pairing_codes(h.now())?, 0);
    Ok(())
}

#[tokio::test]
async fn pairing_works_without_javascript() -> TestResult {
    let h = harness();
    let code = h.fresh_code();
    let res = h
        .send(
            Rb::post("/api/pair")
                .hdr(
                    "user-agent",
                    "Mozilla/5.0 (Linux; Android 14; Pixel 8) Mobile Safari/537",
                )
                .form(&format!("code={}&device_name=", display_code(&code))),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.header("location"), Some("/"));
    let token = res
        .set_cookies()
        .first()
        .and_then(|c| c.strip_prefix("craft_session="))
        .and_then(|r| r.split(';').next())
        .expect("cookie")
        .to_owned();
    let home = h.send(Rb::get("/").cookie(&token)).await;
    assert_eq!(home.status, StatusCode::OK);
    let html = home.text();
    assert!(html.contains("Paired as"), "{html}");
    assert!(
        html.contains("Android phone"),
        "default name from the user agent"
    );
    assert!(html.contains("/api/logout") && html.contains("name=\"csrf_token\""));
    assert!(html.contains("hx-headers") && html.contains("X-CSRF-Token"));
    Ok(())
}

#[tokio::test]
async fn device_names_are_cleaned() -> TestResult {
    let h = harness();
    let code = h.fresh_code();
    let long = "x".repeat(100);
    let res = h
        .send(Rb::post("/api/pair").form(&format!("code={code}&device_name=%20%20{long}%07%1B%20")))
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let rows = h.db().list_sessions(h.now())?;
    assert_eq!(
        rows.first().map(|r| r.device_name.as_str()),
        Some("x".repeat(40).as_str())
    );
    Ok(())
}

#[tokio::test]
async fn bad_codes_all_get_the_same_generic_answer() -> TestResult {
    let h = harness();
    let used = h.fresh_code();
    let ok = h
        .send(Rb::post("/api/pair").json(&format!(r#"{{"code":"{used}"}}"#)))
        .await;
    assert_eq!(ok.status, StatusCode::OK);
    let expired = h.fresh_code();
    h.advance(601);
    let mut answers = Vec::new();
    for (label, code) in [
        ("reused", used.as_str()),
        ("expired", expired.as_str()),
        ("wrong", "ZZZZZZZZ"),
        ("empty", ""),
        ("too long", &"A".repeat(500)),
    ] {
        h.advance(30); // stay under the per-IP limit
        let res = h
            .send(
                Rb::post("/api/pair")
                    .peer(&format!("10.0.0.{}:1", answers.len() + 1))
                    .json(&format!(r#"{{"code":"{code}"}}"#)),
            )
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}");
        assert_eq!(
            res.err_message(),
            "That code is not valid or has expired",
            "{label}"
        );
        answers.push(res.text());
    }
    assert!(
        answers.windows(2).all(|w| w.first() == w.get(1)),
        "identical bodies"
    );
    // The form flow shows the same message on the pair page, with a 400.
    let form = h.send(Rb::post("/api/pair").form("code=ZZZZZZZZ")).await;
    assert_eq!(form.status, StatusCode::BAD_REQUEST);
    assert!(
        form.text()
            .contains("That code is not valid or has expired")
    );
    assert!(
        form.text().contains("name=\"code\""),
        "the form is shown again"
    );
    Ok(())
}

#[tokio::test]
async fn malformed_pairing_bodies_are_rejected_politely() -> TestResult {
    let h = harness();
    let cases: Vec<(&str, Rb)> = vec![
        ("empty json", Rb::post("/api/pair").json("")),
        ("json garbage", Rb::post("/api/pair").json("{{{")),
        (
            "json wrong type",
            Rb::post("/api/pair").json(r#"{"code":5}"#),
        ),
        (
            "form without code",
            Rb::post("/api/pair").form("device_name=x"),
        ),
        ("form not utf-8", Rb::post("/api/pair").form("code=%FF%FE")),
        (
            "no content type",
            Rb::post("/api/pair").body(b"code=AAAAAAAA"),
        ),
    ];
    for (label, rb) in cases {
        let res = h.send(rb.peer(&format!("10.1.{}.1:1", label.len()))).await;
        assert!(
            matches!(res.status, StatusCode::BAD_REQUEST),
            "{label}: {}",
            res.status
        );
    }
    Ok(())
}

#[tokio::test]
async fn per_ip_rate_limit_returns_429_with_retry_after() -> TestResult {
    let h = harness();
    for i in 0..5 {
        let res = h
            .send(Rb::post("/api/pair").json(r#"{"code":"AAAAAAAA"}"#))
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "attempt {i}");
    }
    let res = h
        .send(Rb::post("/api/pair").json(r#"{"code":"AAAAAAAA"}"#))
        .await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    let retry: i64 = res
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .expect("Retry-After");
    assert!((1..=120).contains(&retry), "{retry}");
    assert_eq!(res.err_code(), "rate_limited");
    // Even a correct code is refused while locked out.
    let code = h.fresh_code();
    let locked = h
        .send(Rb::post("/api/pair").json(&format!(r#"{{"code":"{code}"}}"#)))
        .await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
    // Another phone is unaffected.
    let other = h
        .send(
            Rb::post("/api/pair")
                .peer("192.168.1.77:1")
                .json(&format!(r#"{{"code":"{code}"}}"#)),
        )
        .await;
    assert_eq!(other.status, StatusCode::OK);
    // The form flow shows the problem on the page, with the header.
    let form = h.send(Rb::post("/api/pair").form("code=AAAAAAAA")).await;
    assert_eq!(form.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(form.header("retry-after").is_some());
    assert!(form.text().contains("Too many attempts"));
    // After the lockout the first phone can try again.
    h.advance(200);
    let code2 = h.fresh_code();
    let again = h
        .send(Rb::post("/api/pair").json(&format!(r#"{{"code":"{code2}"}}"#)))
        .await;
    assert_eq!(again.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn global_lockout_wipes_codes_and_blocks_pairing() -> TestResult {
    let h = harness();
    let victim = h.fresh_code();
    for i in 0..20u8 {
        let res = h
            .send(
                Rb::post("/api/pair")
                    .peer(&format!("10.2.0.{}:1", i + 1))
                    .json(r#"{"code":"AAAAAAAA"}"#),
            )
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "attempt {i}");
    }
    assert_eq!(
        h.db().count_pairing_codes(h.now())?,
        0,
        "pending codes were wiped"
    );
    let res = h
        .send(
            Rb::post("/api/pair")
                .peer("10.3.0.1:1")
                .json(&format!(r#"{{"code":"{victim}"}}"#)),
        )
        .await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS);
    let retry: i64 = res
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .expect("Retry-After");
    assert!(retry > 500 && retry <= 600, "{retry}");
    h.advance(601);
    let fresh = h.fresh_code();
    let res = h
        .send(
            Rb::post("/api/pair")
                .peer("10.3.0.2:1")
                .json(&format!(r#"{{"code":"{fresh}"}}"#)),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    Ok(())
}

// ---------------------------------------------------------------- sessions

#[tokio::test]
async fn session_expiry_slides_but_writes_rarely() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    let row = || -> Result<crate::db::SessionRow, Box<dyn std::error::Error>> {
        Ok(h.db()
            .list_sessions(h.now())?
            .into_iter()
            .next()
            .ok_or("no session")?)
    };
    assert_eq!(row()?.last_seen_unix, T0);
    // Within the touch interval: no write, no new cookie.
    h.advance(100);
    let quiet = h.send(Rb::get("/").cookie(&s.token)).await;
    assert_eq!(quiet.status, StatusCode::OK);
    assert!(quiet.set_cookies().is_empty());
    assert_eq!(row()?.last_seen_unix, T0);
    // After it: expiry slides and the cookie lifetime is renewed.
    h.advance(250);
    let touched = h.send(Rb::get("/").cookie(&s.token)).await;
    assert_eq!(touched.status, StatusCode::OK);
    assert!(
        touched
            .set_cookies()
            .first()
            .is_some_and(|c| c.ends_with("Max-Age=2592000")
                && c.starts_with(&format!("craft_session={}", s.token)))
    );
    assert_eq!(row()?.last_seen_unix, T0 + 350);
    assert_eq!(row()?.expires_unix, T0 + 350 + SESSION_TTL);
    // Unused for the whole lifetime: gone, and the stale cookie is cleared.
    h.advance(SESSION_TTL + 1);
    let gone = h.send(Rb::get("/").cookie(&s.token)).await;
    assert_eq!(gone.status, StatusCode::SEE_OTHER);
    assert_eq!(gone.header("location"), Some("/pair"));
    assert!(gone.set_cookies().iter().any(|c| c.contains("Max-Age=0")));
    Ok(())
}

#[tokio::test]
async fn revocation_takes_effect_on_the_next_request() -> TestResult {
    let h = harness();
    let a = h.pair("Pixel").await;
    let b = h.pair("iPad").await;
    // The list has public ids and metadata only.
    let list = h.send(Rb::get("/api/sessions").cookie(&a.token)).await;
    assert_eq!(list.status, StatusCode::OK);
    let text = list.text();
    for secret in [
        &a.token,
        &b.token,
        &a.csrf,
        &b.csrf,
        &hash_secret(&a.token),
        &hash_secret(&b.token),
    ] {
        assert!(!text.contains(secret.as_str()), "list leaks a secret");
    }
    assert!(!text.contains("token") && !text.contains("csrf"));
    assert_eq!(list.json().as_array().map(Vec::len), Some(2));
    // A revokes B.
    assert_eq!(
        h.send(Rb::get("/").cookie(&b.token)).await.status,
        StatusCode::OK
    );
    let del = h
        .send(
            Rb::delete(&format!("/api/sessions/{}", b.id))
                .cookie(&a.token)
                .csrf(&a.csrf),
        )
        .await;
    assert_eq!(del.status, StatusCode::OK);
    assert!(
        del.set_cookies().is_empty(),
        "revoking another device keeps our cookie"
    );
    let nav = h.send(Rb::get("/").cookie(&b.token)).await;
    assert_eq!(nav.status, StatusCode::SEE_OTHER);
    let api = h.send(Rb::get("/api/sessions").cookie(&b.token)).await;
    assert_eq!(api.status, StatusCode::UNAUTHORIZED);
    let htmx = h
        .send(
            Rb::get("/settings/devices")
                .cookie(&b.token)
                .hdr("hx-request", "true"),
        )
        .await;
    assert_eq!(htmx.status, StatusCode::UNAUTHORIZED);
    // B's old token cannot revoke anything.
    let sneaky = h
        .send(
            Rb::delete(&format!("/api/sessions/{}", a.id))
                .cookie(&b.token)
                .csrf(&b.csrf),
        )
        .await;
    assert_eq!(sneaky.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.send(Rb::get("/").cookie(&a.token)).await.status,
        StatusCode::OK
    );
    // Unknown id.
    let missing = h
        .send(
            Rb::delete("/api/sessions/0123456789abcdef")
                .cookie(&a.token)
                .csrf(&a.csrf),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    // Revoking the current device is a logout.
    let own = h
        .send(
            Rb::delete(&format!("/api/sessions/{}", a.id))
                .cookie(&a.token)
                .csrf(&a.csrf),
        )
        .await;
    assert_eq!(own.status, StatusCode::OK);
    assert!(
        own.set_cookies()
            .iter()
            .any(|c| c.starts_with("craft_session=;") && c.contains("Max-Age=0"))
    );
    assert_eq!(
        h.send(Rb::get("/").cookie(&a.token)).await.status,
        StatusCode::SEE_OTHER
    );
    assert!(h.db().list_sessions(h.now())?.is_empty());
    Ok(())
}

#[tokio::test]
async fn logout_deletes_the_session_and_clears_the_cookie() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    let res = h
        .send(Rb::post("/api/logout").cookie(&s.token).csrf(&s.csrf))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.set_cookies(),
        ["craft_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"]
    );
    assert_eq!(
        h.send(Rb::get("/").cookie(&s.token)).await.status,
        StatusCode::SEE_OTHER
    );
    Ok(())
}

#[tokio::test]
async fn devices_page_lists_and_revokes_with_plain_forms() -> TestResult {
    let h = harness();
    let a = h.pair("Pixel <b>").await;
    let b = h.pair("iPad").await;
    let page = h.send(Rb::get("/settings/devices").cookie(&a.token)).await;
    assert_eq!(page.status, StatusCode::OK);
    let html = page.text();
    assert!(
        html.contains("Pixel &#60;b&#62;") || html.contains("Pixel &lt;b&gt;"),
        "names are escaped: {html}"
    );
    assert!(!html.contains("<b>"));
    assert!(html.contains(&format!("/api/sessions/{}/revoke", b.id)));
    let res = h
        .send(
            Rb::post(&format!("/api/sessions/{}/revoke", b.id))
                .cookie(&a.token)
                .form(&format!("csrf_token={}", a.csrf)),
        )
        .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(res.header("location"), Some("/settings/devices"));
    assert_eq!(
        h.send(Rb::get("/").cookie(&b.token)).await.status,
        StatusCode::SEE_OTHER
    );
    Ok(())
}

#[tokio::test]
async fn unauthenticated_requests_get_a_redirect_or_a_401() -> TestResult {
    let h = harness();
    let nav = h.send(Rb::get("/")).await;
    assert_eq!(
        (nav.status, nav.header("location")),
        (StatusCode::SEE_OTHER, Some("/pair"))
    );
    let devices = h.send(Rb::get("/settings/devices")).await;
    assert_eq!(devices.status, StatusCode::SEE_OTHER);
    for (label, rb) in [
        ("htmx", Rb::get("/").hdr("hx-request", "true")),
        (
            "accept json",
            Rb::get("/").hdr("accept", "application/json"),
        ),
        ("api path", Rb::get("/api/sessions")),
        ("post", Rb::post("/api/logout")),
    ] {
        let res = h.send(rb).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{label}");
    }
    let htmx = h.send(Rb::get("/").hdr("hx-request", "true")).await;
    assert_eq!(htmx.header("hx-redirect"), Some("/pair"));
    // Garbage cookies are treated as no cookie.
    for cookie in [
        "craft_session=",
        "craft_session=zz",
        "craft_session=%00",
        "a=b",
    ] {
        let res = h.send(Rb::get("/").hdr("cookie", cookie)).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{cookie}");
    }
    Ok(())
}

// ------------------------------------------------------------ route table

/// Every route of the panel with its access level and methods. Adding a route
/// to `routes::table` without listing it here fails
/// `route_table_matches_the_expected_list`, so a new route cannot silently
/// skip the access decision.
const EXPECTED_ROUTES: &[(&str, Access, &[Method])] = &[
    ("/healthz", Access::Public, &[Method::GET]),
    ("/static/{*file}", Access::Public, &[Method::GET]),
    ("/pair", Access::Public, &[Method::GET]),
    ("/api/pair", Access::Pairing, &[Method::POST]),
    ("/", Access::Session, &[Method::GET]),
    ("/world", Access::Session, &[Method::GET]),
    ("/partials/world", Access::Session, &[Method::GET]),
    ("/world/upload", Access::Session, &[Method::POST]),
    (
        "/world/import",
        Access::Session,
        &[Method::GET, Method::POST],
    ),
    ("/world/discard", Access::Session, &[Method::POST]),
    ("/partials/server-card", Access::Session, &[Method::GET]),
    ("/server/start", Access::Session, &[Method::POST]),
    ("/server/stop", Access::Session, &[Method::POST]),
    ("/server/restart", Access::Session, &[Method::POST]),
    (
        "/server/reset",
        Access::Session,
        &[Method::GET, Method::POST],
    ),
    (
        "/server/delete",
        Access::Session,
        &[Method::GET, Method::POST],
    ),
    ("/create", Access::Session, &[Method::GET]),
    ("/server/create", Access::Session, &[Method::POST]),
    ("/backups", Access::Session, &[Method::GET]),
    ("/partials/backups", Access::Session, &[Method::GET]),
    ("/backups/create", Access::Session, &[Method::POST]),
    (
        "/backups/{id}/restore",
        Access::Session,
        &[Method::GET, Method::POST],
    ),
    ("/players", Access::Session, &[Method::GET]),
    ("/players/whitelist/add", Access::Session, &[Method::POST]),
    (
        "/players/whitelist/remove",
        Access::Session,
        &[Method::POST],
    ),
    ("/players/kick", Access::Session, &[Method::POST]),
    ("/logs", Access::Session, &[Method::GET]),
    ("/logs/tail", Access::Session, &[Method::GET]),
    ("/mods", Access::Session, &[Method::GET]),
    ("/partials/mods", Access::Session, &[Method::GET]),
    ("/mods/updates", Access::Session, &[Method::GET]),
    (
        "/mods/install/{id}",
        Access::Session,
        &[Method::GET, Method::POST],
    ),
    (
        "/mods/remove/{id}",
        Access::Session,
        &[Method::GET, Method::POST],
    ),
    ("/performance", Access::Session, &[Method::GET]),
    ("/partials/performance", Access::Session, &[Method::GET]),
    (
        "/performance/view-distance",
        Access::Session,
        &[Method::POST],
    ),
    ("/online", Access::Session, &[Method::GET]),
    ("/partials/online", Access::Session, &[Method::GET]),
    (
        "/online/enable",
        Access::Session,
        &[Method::GET, Method::POST],
    ),
    ("/online/disable", Access::Session, &[Method::POST]),
    ("/settings/devices", Access::Session, &[Method::GET]),
    ("/api/logout", Access::Session, &[Method::POST]),
    ("/api/sessions", Access::Session, &[Method::GET]),
    ("/api/sessions/{id}", Access::Session, &[Method::DELETE]),
    (
        "/api/sessions/{id}/revoke",
        Access::Session,
        &[Method::POST],
    ),
];

fn concrete(path: &str) -> String {
    path.replace("{*file}", "panel.css")
        .replace("{id}", "0123456789abcdef")
}

#[test]
fn route_table_matches_the_expected_list() {
    let mut actual = routes::table::<FakeRuntime, NoProbe>().specs();
    let mut expected: Vec<(&str, Access)> =
        EXPECTED_ROUTES.iter().map(|(p, a, _)| (*p, *a)).collect();
    actual.sort_by_key(|(p, _)| *p);
    expected.sort_by_key(|(p, _)| *p);
    assert_eq!(
        actual, expected,
        "routes::table and EXPECTED_ROUTES disagree: declare the access level of every route"
    );
}

#[tokio::test]
async fn every_route_enforces_its_declared_access() -> TestResult {
    let h = harness();
    let all = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
    ];
    for (path, access, methods) in EXPECTED_ROUTES {
        let url = concrete(path);
        for method in &all {
            let res = h.send(Rb::new(method.clone(), &url)).await;
            let registered = methods.contains(method);
            let label = format!("{method} {path}");
            if !registered {
                assert!(
                    matches!(
                        res.status,
                        StatusCode::METHOD_NOT_ALLOWED
                            | StatusCode::UNAUTHORIZED
                            | StatusCode::SEE_OTHER
                    ),
                    "{label}: {}",
                    res.status
                );
                assert_ne!(res.status, StatusCode::OK, "{label}");
                continue;
            }
            match access {
                Access::Session => assert!(
                    matches!(res.status, StatusCode::UNAUTHORIZED | StatusCode::SEE_OTHER),
                    "{label} must require a session, got {}",
                    res.status
                ),
                Access::Public | Access::Pairing => {
                    assert_ne!(res.status, StatusCode::UNAUTHORIZED, "{label}");
                    assert_ne!(res.status, StatusCode::SEE_OTHER, "{label}");
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_route_missing_from_the_table_still_requires_a_session() -> TestResult {
    let h = harness();
    let core = PanelCore::new(
        h.db().clone(),
        Arc::new(Config::default()),
        Arc::new({
            let c = Arc::clone(&h.clock);
            move || c.load(Ordering::SeqCst)
        }),
    );
    let rogue: Router = super::global_layers(
        super::guarded(
            Router::new()
                .route(
                    "/rogue",
                    get(|| async { "secret" }).post(|| async { "secret" }),
                )
                .with_state(()),
            &core,
            HashMap::new(),
        ),
        &core,
    );
    for method in [Method::GET, Method::POST] {
        let res = rogue
            .clone()
            .oneshot(Rb::new(method.clone(), "/rogue").build())
            .await
            .expect("infallible");
        assert!(
            matches!(
                res.status(),
                StatusCode::SEE_OTHER | StatusCode::UNAUTHORIZED
            ),
            "{method}"
        );
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16).await?;
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
    }
    // With a real session it is served (and a POST still needs CSRF).
    let s = h.pair("Pixel").await;
    let ok = rogue
        .clone()
        .oneshot(Rb::get("/rogue").cookie(&s.token).build())
        .await?;
    assert_eq!(ok.status(), StatusCode::OK);
    let no_csrf = rogue
        .clone()
        .oneshot(Rb::post("/rogue").cookie(&s.token).build())
        .await?;
    assert_eq!(no_csrf.status(), StatusCode::FORBIDDEN);
    Ok(())
}

// ----------------------------------------------------------------- assets

#[tokio::test]
async fn static_assets_are_served_from_memory_with_caching() -> TestResult {
    let h = harness();
    let css = h.send(Rb::get("/static/panel.css")).await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.header("content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(css.header("cache-control"), Some("public, max-age=3600"));
    assert!(css.text().contains("prefers-color-scheme: dark"));
    let js = h.send(Rb::get("/static/htmx.min.js")).await;
    assert_eq!(js.status, StatusCode::OK);
    assert_eq!(
        js.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert!(js.body.len() > 10_000 && js.text().contains("htmx"));
    // Unknown names, traversal attempts and non-GET methods.
    for path in [
        "/static/nope.js",
        "/static/",
        "/static/../Cargo.toml",
        "/static/%2e%2e/x",
        "/static/panel.css/x",
    ] {
        let res = h.send(Rb::get(path)).await;
        assert!(
            matches!(res.status, StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST),
            "{path}: {}",
            res.status
        );
    }
    let post = h.send(Rb::post("/static/panel.css")).await;
    assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);
    // The embedded copy matches the documented hash.
    let readme = include_str!("../../static/README.md");
    let digest = hash_secret_bytes(&js.body);
    assert!(
        readme.contains(&digest),
        "static/README.md must list the sha256 of htmx.min.js ({digest})"
    );
    Ok(())
}

fn hash_secret_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[tokio::test]
async fn pages_have_no_inline_script_style_or_handlers() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    let pages = [
        h.send(Rb::get("/pair")).await,
        h.send(Rb::get("/").cookie(&s.token)).await,
        h.send(Rb::get("/settings/devices").cookie(&s.token)).await,
        h.send(Rb::get("/missing")).await,
        h.send(Rb::post("/api/pair").form("code=ZZZZZZZZ")).await,
    ];
    for page in pages {
        let html = page.text();
        assert!(
            html.contains("Not an official Minecraft product."),
            "footer"
        );
        assert!(html.contains("/static/htmx.min.js") && html.contains("/static/panel.css"));
        assert!(!html.contains("<style"), "{html}");
        assert!(!html.contains(" style="), "{html}");
        for (i, _) in html.match_indices("<script") {
            let tag = html
                .get(i..)
                .and_then(|r| r.split('>').next())
                .unwrap_or_default();
            assert!(tag.contains(" src=\"/static/"), "inline script: {tag}");
        }
        let lower = html.to_lowercase();
        for handler in [
            " onclick=",
            " onload=",
            " onerror=",
            " onsubmit=",
            " onchange=",
            "javascript:",
        ] {
            assert!(!lower.contains(handler), "{handler}");
        }
        assert!(html.contains("width=device-width"));
    }
    let pair = h.send(Rb::get("/pair")).await.text();
    for attr in [
        "autocapitalize=\"characters\"",
        "autocomplete=\"off\"",
        "inputmode=\"text\"",
        "method=\"post\"",
        "action=\"/api/pair\"",
        "home network",
    ] {
        assert!(pair.contains(attr), "{attr}");
    }
    Ok(())
}

// ------------------------------------------------------------- hardening

#[tokio::test]
async fn bodies_over_the_limit_are_refused() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    let big = vec![b'a'; 20 * 1024];
    // Declared too large.
    let declared = h
        .send(
            Rb::post("/api/pair")
                .json("{}")
                .hdr("content-length", "999999"),
        )
        .await;
    assert_eq!(declared.status, StatusCode::PAYLOAD_TOO_LARGE);
    // Undeclared (streamed) and actually too large, on a public and a session route.
    let streamed = h
        .send(
            Rb::post("/api/pair")
                .peer("10.9.9.9:1")
                .json("{}")
                .body(&big),
        )
        .await;
    assert_eq!(streamed.status, StatusCode::PAYLOAD_TOO_LARGE);
    let form_body = format!("csrf_token={}&pad={}", s.csrf, "a".repeat(20 * 1024));
    let session = h
        .send(Rb::post("/api/logout").cookie(&s.token).form(&form_body))
        .await;
    assert_eq!(session.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(session.err_code(), "payload_too_large");
    // Just under the limit is fine.
    let ok_body = format!("csrf_token={}&pad={}", s.csrf, "a".repeat(8 * 1024));
    let ok = h
        .send(Rb::post("/api/logout").cookie(&s.token).form(&ok_body))
        .await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER);
    Ok(())
}

#[tokio::test]
async fn methods_that_are_not_routed_get_405() -> TestResult {
    let h = harness();
    for (method, path) in [
        (Method::PUT, "/pair"),
        (Method::DELETE, "/pair"),
        (Method::PATCH, "/healthz"),
        (Method::POST, "/healthz"),
        (Method::GET, "/api/pair"),
        (Method::OPTIONS, "/api/pair"),
        (Method::TRACE, "/pair"),
        (Method::CONNECT, "/pair"),
    ] {
        let res = h.send(Rb::new(method.clone(), path)).await;
        assert!(
            matches!(
                res.status,
                StatusCode::METHOD_NOT_ALLOWED | StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
            ),
            "{method} {path}: {}",
            res.status
        );
        assert_ne!(res.status, StatusCode::OK);
    }
    Ok(())
}

#[tokio::test]
async fn weird_requests_never_panic_or_leak() -> TestResult {
    let h = harness();
    let long_cookie = format!("craft_session={}", "a".repeat(9000));
    let cases: Vec<(&str, Rb)> = vec![
        ("nul in path", Rb::get("/%00")),
        ("traversal", Rb::get("/..%2f..%2fetc/passwd")),
        ("double slash", Rb::get("//")),
        ("long path", Rb::get(&format!("/{}", "a".repeat(5000)))),
        (
            "non-utf8 cookie",
            Rb::get("/").raw_hdr("cookie", &[b'a', b'=', 0xff, 0xfe]),
        ),
        ("huge cookie", Rb::get("/").hdr("cookie", &long_cookie)),
        ("non-utf8 accept", Rb::get("/").raw_hdr("accept", &[0xff])),
        (
            "non-utf8 user agent",
            Rb::post("/api/pair")
                .raw_hdr("user-agent", &[0xff])
                .form("code=ZZZZZZZZ"),
        ),
        (
            "non-utf8 content type",
            Rb::post("/api/pair")
                .raw_hdr("content-type", &[0xff])
                .body(b"x"),
        ),
        (
            "non-utf8 csrf header",
            Rb::post("/api/logout")
                .cookie(&"a".repeat(64))
                .raw_hdr("x-csrf-token", &[0xff]),
        ),
        (
            "huge csrf header",
            Rb::post("/api/logout")
                .cookie(&"a".repeat(64))
                .csrf(&"b".repeat(9000)),
        ),
        (
            "sql in id",
            Rb::delete("/api/sessions/1%27%20OR%201=1--")
                .cookie(&"a".repeat(64))
                .csrf("x"),
        ),
        (
            "emoji device name",
            Rb::post("/api/pair").form("code=ZZZZZZZZ&device_name=%F0%9F%98%80"),
        ),
    ];
    for (label, rb) in cases {
        let res = h.send(rb).await;
        let text = res.text();
        assert!(!res.status.is_server_error(), "{label}: {}", res.status);
        assert_security_headers(&res, label);
        for leak in ["panicked", "rusqlite", "SQL", "src\\", "src/", "Backtrace"] {
            assert!(!text.contains(leak), "{label}: {text}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn error_responses_are_json_for_apis_and_pages_for_browsers() -> TestResult {
    let h = harness();
    let api = h.send(Rb::get("/api/nothing")).await;
    assert_eq!(api.status, StatusCode::NOT_FOUND);
    assert_eq!(api.header("content-type"), Some("application/json"));
    assert_eq!(api.err_code(), "not_found");
    let page = h.send(Rb::get("/nothing")).await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    assert!(
        page.header("content-type")
            .is_some_and(|c| c.starts_with("text/html"))
    );
    assert!(page.text().contains("Nothing here"));
    let json_wanted = h
        .send(Rb::get("/nothing").hdr("accept", "application/json"))
        .await;
    assert_eq!(json_wanted.header("content-type"), Some("application/json"));
    Ok(())
}

// ---------------------------------------------------------------- secrets

// The log-redaction flow lives in `tests/panel_secrets.rs`: it needs its own
// process because a thread-local tracing subscriber races with the callsite
// interest cache when other tests log concurrently.

#[test]
fn debug_output_of_the_session_hides_the_csrf_token() {
    let dbg = format!(
        "{:?}",
        auth::AuthSession {
            id: "i".into(),
            device_name: "d".into(),
            csrf_token: "supersecret".into()
        }
    );
    assert!(!dbg.contains("supersecret"));
}

// ------------------------------------------------------------------ misc

#[test]
fn pairing_urls_use_hostname_and_lan_address() {
    let cfg = Config::default();
    let (url, alt) = super::pairing_urls(&cfg, Some("192.168.1.9".parse().expect("ip")));
    assert_eq!(url, "http://craft-a-server.local:8080/pair");
    assert_eq!(alt, ["http://192.168.1.9:8080/pair"]);
    let (_, none) = super::pairing_urls(&cfg, Some("8.8.8.8".parse().expect("ip")));
    assert!(none.is_empty(), "a public address is never advertised");
    let (_, none) = super::pairing_urls(&cfg, None);
    assert!(none.is_empty());
    let cfg80 = Config {
        panel_port: 80,
        ..Config::default()
    };
    assert_eq!(
        super::pairing_urls(&cfg80, None).0,
        "http://craft-a-server.local/pair"
    );
}

#[tokio::test]
async fn healthz_is_public_and_tiny() -> TestResult {
    let h = harness();
    let res = h.send(Rb::get("/healthz")).await;
    assert_eq!((res.status, res.text().as_str()), (StatusCode::OK, "ok"));
    Ok(())
}

mod online;
mod performance;
mod screens;
mod world;
