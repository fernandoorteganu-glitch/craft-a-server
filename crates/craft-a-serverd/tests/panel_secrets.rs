//! Secrets must never reach the logs. This runs in its own process (one test
//! file, one test) so the tracing subscriber sees every event deterministically.
// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use craft_a_server_core::config::Config;
use craft_a_server_core::mc_protocol::ServerStatus;
use craft_a_serverd::db::{Db, hash_secret};
use craft_a_serverd::lifecycle::{DiskUsage, Env, Service, StatusProbe};
use craft_a_serverd::panel::auth::{display_code, issue_pairing_code};
use craft_a_serverd::panel::{PanelState, router};
use craft_a_serverd::runtime::FakeRuntime;
use tower::ServiceExt;

struct NoProbe;

impl StatusProbe for NoProbe {
    async fn status(&self, _port: u16) -> Option<ServerStatus> {
        None
    }
}

#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn request(
    method: &str,
    path: &str,
    peer: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "craft-a-server.local:8080");
    if method != "GET" {
        b = b.header("origin", "http://craft-a-server.local:8080");
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::from(body.to_owned())).expect("request");
    let peer: SocketAddr = peer.parse().expect("peer");
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

#[tokio::test]
async fn secrets_never_reach_the_logs() {
    let buf = LogBuf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let env = Env {
        total_ram_bytes: 8 << 30,
        uid: 1000,
        gid: 1000,
        now_unix: Arc::new(|| 1_700_000_000),
        disk: Arc::new(|_| {
            Ok(DiskUsage {
                free_bytes: 90,
                total_bytes: 100,
            })
        }),
        poll_interval: std::time::Duration::from_millis(10),
    };
    let svc = Arc::new(Service::new(
        Arc::new(Config::default()),
        Db::open_in_memory().expect("db"),
        Arc::new(FakeRuntime::new()),
        Arc::new(NoProbe),
        env,
    ));
    let app = router(PanelState::new(Arc::clone(&svc)));
    let send = |req: Request<Body>| {
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.expect("infallible");
            let (parts, body) = res.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 20).await.expect("body");
            (parts, bytes)
        }
    };

    let code = issue_pairing_code(svc.db(), 1_700_000_000, 600)
        .expect("code")
        .code;
    let lan = "192.168.1.20:5000";
    let (parts, body) = send(request(
        "POST",
        "/api/pair",
        lan,
        &[("content-type", "application/json")],
        &format!(r#"{{"code":"{code}","device_name":"Pixel"}}"#),
    ))
    .await;
    assert_eq!(parts.status, StatusCode::OK);
    let set_cookie = parts
        .headers
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("cookie");
    let token = set_cookie
        .strip_prefix("craft_session=")
        .and_then(|r| r.split(';').next())
        .expect("token")
        .to_owned();
    let csrf = serde_json::from_slice::<serde_json::Value>(&body)
        .expect("json")
        .get("csrf_token")
        .and_then(|v| v.as_str())
        .expect("csrf")
        .to_owned();

    // Success, wrong CSRF, a stale cookie, a rate-limited guesser, then logout.
    let cookie = format!("craft_session={token}");
    send(request("GET", "/", lan, &[("cookie", &cookie)], "")).await;
    send(request(
        "POST",
        "/api/logout",
        lan,
        &[("cookie", &cookie), ("x-csrf-token", "wrong-csrf-value")],
        "",
    ))
    .await;
    let stale = format!("craft_session={}", "c".repeat(64));
    send(request("GET", "/", lan, &[("cookie", &stale)], "")).await;
    for _ in 0..7 {
        send(request(
            "POST",
            "/api/pair",
            "10.8.8.8:1",
            &[("content-type", "application/json")],
            r#"{"code":"WRONGCODE"}"#,
        ))
        .await;
    }
    send(request(
        "POST",
        "/api/logout",
        lan,
        &[("cookie", &cookie), ("x-csrf-token", &csrf)],
        "",
    ))
    .await;

    let log = String::from_utf8_lossy(&buf.0.lock().expect("log buffer")).into_owned();
    assert!(
        log.contains("panel request"),
        "the access log must be active:\n{log}"
    );
    assert!(log.contains("phone paired"), "{log}");
    for (what, secret) in [
        ("pairing code", code.as_str()),
        ("displayed code", &display_code(&code)),
        ("session token", &token),
        ("token hash", &hash_secret(&token)),
        ("csrf token", &csrf),
        ("wrong csrf value", "wrong-csrf-value"),
        ("guessed code", "WRONGCODE"),
    ] {
        assert!(!log.contains(secret), "{what} leaked into the log:\n{log}");
    }
    assert!(!log.to_lowercase().contains("cookie"), "{log}");
}
