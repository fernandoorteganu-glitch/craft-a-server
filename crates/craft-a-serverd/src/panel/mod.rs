//! The local web panel: security and authentication skeleton plus a minimal UI.
//!
//! This module is the only HTTP surface of the daemon. It is meant for a phone
//! on the home Wi-Fi, over plain HTTP, so every request passes a stack of
//! checks before a handler runs. From the outside in:
//!
//! 1. **security headers** on every response (CSP without `unsafe-inline`,
//!    `nosniff`, no referrer, no framing, `no-store` for dynamic pages);
//! 2. **peer filter**: the TCP peer must be loopback, RFC 1918, link-local or
//!    IPv6 ULA ([`net::peer_allowed`]); anyone else gets a bare 403;
//! 3. **error shaping**: every error response becomes a friendly page or JSON
//!    object with no internals ([`PanelError`]);
//! 4. **Host allow-list** against DNS rebinding ([`net::host_allowed`]);
//! 5. **limits**: body size, request timeout, concurrency;
//! 6. per-route **guard** (route layer): `Origin`/`Referer` check on every
//!    state-changing method, session lookup, and CSRF token check, driven by
//!    the [`routes::Access`] declared for the route in the route table.
//!
//! # For screen authors
//!
//! * State: [`PanelState<R, P>`] (cheap to clone). `state.service()` is the
//!   lifecycle [`Service`]; `state.core()` has the database, config and clock.
//!   Handlers that do not need the service can take `State<Arc<PanelCore>>`
//!   and stay non-generic.
//! * Auth: take an [`AuthSession`] argument. The guard has already
//!   authenticated the request and, for POST/PUT/PATCH/DELETE, verified Origin
//!   and the CSRF token, so handlers do nothing extra. A route that is not in
//!   the route table is treated as "session required" (fail closed).
//! * Long actions (start, stop, backup, restore...) never run inside the
//!   request: use [`activity::spawn_action`], which runs the lifecycle call in
//!   a task and lets screens show progress through [`activity::Activity`].
//! * Add a screen: create a module in `routes/`, write handlers returning
//!   `Result<impl IntoResponse, PanelError>`, add a template in `templates/`
//!   (render it with [`pages::page`]) and register it in [`routes::table`] with
//!   `.add(path, Access::Session, get(handler))`. Add the path to the
//!   `EXPECTED_ROUTES` table in `panel/tests.rs`; that test fails when the
//!   router and the table disagree.
//! * CSRF: the layout puts the token in `<body hx-headers=...>`, so every htmx
//!   request sends `X-CSRF-Token`; plain forms include
//!   `{{ m::csrf_field(csrf_token) }}` (macro in `templates/macros.html`).
//! * No inline scripts, styles or event handlers anywhere: the CSP forbids
//!   them. Put CSS in `static/panel.css` and behaviour in htmx attributes.
//! * Strings shown to people go in `strings.rs`.
//! * Never log cookies, tokens, pairing codes or the CSRF token.

pub mod activity;
pub mod auth;
pub mod error;
mod mapping;
mod middleware;
pub mod net;
pub mod pages;
mod perf_views;
pub mod ratelimit;
pub mod routes;
pub mod strings;
mod views;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::middleware::{from_fn, from_fn_with_state};
use craft_a_server_core::config::Config;
use tokio::sync::Semaphore;

pub use auth::AuthSession;
pub use error::PanelError;
pub use routes::Access;

use crate::db::Db;
use crate::lifecycle::{Service, StatusProbe};
use crate::runtime::ContainerRuntime;

/// Largest request body the panel reads (forms and small JSON only).
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// A request that is still running after this long is answered with 408.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Requests handled at once; more get 503.
pub const MAX_INFLIGHT: usize = 64;
const PURGE_EVERY: Duration = Duration::from_secs(600);

/// What the security layers and the auth handlers need; independent of the
/// container runtime types so those parts stay non-generic.
pub struct PanelCore {
    db: Db,
    cfg: Arc<Config>,
    now: auth::Clock,
    limiter: Mutex<ratelimit::PairLimiter>,
    inflight: Arc<Semaphore>,
    request_seq: AtomicU64,
}

impl std::fmt::Debug for PanelCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelCore")
            .field("panel_hostname", &self.cfg.panel_hostname)
            .finish_non_exhaustive()
    }
}

impl PanelCore {
    pub fn new(db: Db, cfg: Arc<Config>, now: auth::Clock) -> Arc<PanelCore> {
        Arc::new(PanelCore {
            db,
            cfg,
            now,
            limiter: Mutex::new(ratelimit::PairLimiter::new()),
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT)),
            request_seq: AtomicU64::new(0),
        })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Current unix time from the (injectable) clock.
    pub fn now(&self) -> i64 {
        (self.now)()
    }

    /// Runs blocking database work off the async threads.
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, PanelError>
    where
        T: Send + 'static,
        F: FnOnce(&Db) -> Result<T, PanelError> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| PanelError::internal(&e))?
    }

    pub(crate) fn limiter(&self) -> std::sync::MutexGuard<'_, ratelimit::PairLimiter> {
        // A panic while holding the lock cannot leave the limiter in a state
        // that makes it less strict, so carry on.
        self.limiter.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Router state: the security core plus the lifecycle service for screens.
pub struct PanelState<R, P> {
    core: Arc<PanelCore>,
    service: Arc<Service<R, P>>,
    activity: Arc<activity::Activity>,
    /// This machine's LAN address, for the Android `.local` fallback.
    lan_ip: Option<std::net::Ipv4Addr>,
}

impl<R, P> Clone for PanelState<R, P> {
    fn clone(&self) -> Self {
        PanelState {
            core: Arc::clone(&self.core),
            service: Arc::clone(&self.service),
            activity: Arc::clone(&self.activity),
            lan_ip: self.lan_ip,
        }
    }
}

impl<R, P> std::fmt::Debug for PanelState<R, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelState").finish_non_exhaustive()
    }
}

impl<R: ContainerRuntime, P: StatusProbe> PanelState<R, P> {
    /// Builds the state from the running service (database, config and clock
    /// are taken from it, so tests that inject a clock into the service get it
    /// in the panel too).
    pub fn new(service: Arc<Service<R, P>>) -> Self {
        let core = PanelCore::new(
            service.db().clone(),
            Arc::clone(&service.cfg),
            Arc::clone(&service.env.now_unix),
        );
        PanelState {
            core,
            service,
            activity: activity::Activity::new(),
            lan_ip: lan_ipv4(),
        }
    }

    /// Overrides the detected LAN address (tests; machines with several NICs).
    #[must_use]
    pub fn with_lan_ip(mut self, lan_ip: Option<std::net::Ipv4Addr>) -> Self {
        self.lan_ip = lan_ip;
        self
    }

    pub fn activity(&self) -> &Arc<activity::Activity> {
        &self.activity
    }

    pub fn lan_ip(&self) -> Option<std::net::Ipv4Addr> {
        self.lan_ip
    }

    pub fn service(&self) -> &Arc<Service<R, P>> {
        &self.service
    }

    pub fn core(&self) -> &Arc<PanelCore> {
        &self.core
    }
}

impl<R, P> FromRef<PanelState<R, P>> for Arc<PanelCore> {
    fn from_ref(state: &PanelState<R, P>) -> Self {
        Arc::clone(&state.core)
    }
}

/// State of the per-route guard.
#[derive(Clone)]
pub(crate) struct GuardState {
    pub(crate) core: Arc<PanelCore>,
    pub(crate) access: Arc<HashMap<&'static str, Access>>,
}

/// The complete panel, ready to be served.
pub fn router<R, P>(state: PanelState<R, P>) -> Router
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    build(state, routes::table())
}

pub(crate) fn build<R, P>(
    state: PanelState<R, P>,
    table: routes::RouteTable<PanelState<R, P>>,
) -> Router
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let core = Arc::clone(&state.core);
    let access = table.access_map();
    let routed = guarded(table.into_router(), &core, access).with_state(state);
    global_layers(routed, &core)
}

/// Adds the per-route guard (Origin, session, CSRF) to every route of `router`.
pub(crate) fn guarded<S>(
    router: Router<S>,
    core: &Arc<PanelCore>,
    access: HashMap<&'static str, Access>,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.route_layer(from_fn_with_state(
        GuardState {
            core: Arc::clone(core),
            access: Arc::new(access),
        },
        middleware::guard,
    ))
}

/// Headers, peer filter, error shaping, Host check and limits.
pub(crate) fn global_layers(router: Router, core: &Arc<PanelCore>) -> Router {
    router
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(from_fn_with_state(Arc::clone(core), middleware::limits))
        .layer(from_fn_with_state(Arc::clone(core), middleware::host_check))
        .layer(from_fn(middleware::shape_errors))
        .layer(from_fn(middleware::peer_filter))
        .layer(from_fn_with_state(Arc::clone(core), middleware::trace))
        .layer(from_fn(middleware::security_headers))
}

/// Serves the panel on `listener` until `shutdown` resolves, then lets
/// in-flight requests finish.
pub async fn serve<R, P, F>(
    state: PanelState<R, P>,
    listener: tokio::net::TcpListener,
    shutdown: F,
) -> io::Result<()>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "web panel listening");
    let purge = {
        let core = Arc::clone(&state.core);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PURGE_EVERY);
            loop {
                tick.tick().await;
                let now = core.now();
                let _ = core
                    .blocking(move |db| {
                        db.purge_expired_sessions(now)?;
                        db.purge_expired_pairing_codes(now)?;
                        Ok(())
                    })
                    .await;
            }
        })
    };
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
    purge.abort();
    tracing::info!("web panel stopped");
    result
}

/// Binds the panel's TCP listener from the configuration.
pub async fn bind(cfg: &Config) -> io::Result<tokio::net::TcpListener> {
    let ip: std::net::IpAddr = cfg.panel_bind.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "panel_bind is not an IP address",
        )
    })?;
    tokio::net::TcpListener::bind(SocketAddr::new(ip, cfg.panel_port)).await
}

/// The pairing page URL by hostname, plus the same by LAN address (for
/// networks where `.local` names do not resolve). Pure: pass the machine's
/// LAN address from [`lan_ipv4`].
pub fn pairing_urls(cfg: &Config, lan_ip: Option<std::net::Ipv4Addr>) -> (String, Vec<String>) {
    let port = if cfg.panel_port == 80 {
        String::new()
    } else {
        format!(":{}", cfg.panel_port)
    };
    let url = format!("http://{}{port}/pair", cfg.panel_hostname);
    let alt = lan_ip
        .filter(|ip| net::peer_allowed(std::net::IpAddr::V4(*ip)) && !ip.is_loopback())
        .map(|ip| format!("http://{ip}{port}/pair"))
        .into_iter()
        .collect();
    (url, alt)
}

/// The IPv4 address this machine uses towards the LAN, found without sending
/// anything (connecting a UDP socket only selects a route).
pub fn lan_ipv4() -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}
