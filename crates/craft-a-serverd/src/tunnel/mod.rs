//! Optional online play through a playit.gg tunnel.
//!
//! The tunnel is **off by default**. Turning it on needs an explicit
//! confirmation (the caller shows [`craft_a_server_core::tunnel::disclosure`]
//! first); the confirmation time is stored. While it is on, a supervisor loop
//! ([`TunnelManager::run`]) keeps the following true, one small step per
//! [`TunnelManager::tick`]:
//!
//! 1. the pinned agent and its command-line frontend are installed and match
//!    their embedded SHA-256 (`install`), re-checked before every launch;
//! 2. the agent runs as our child (`process`), restarted with exponential
//!    backoff when it dies, never with the secret on its command line;
//! 3. if the agent has no secret yet, the claim program runs and the claim
//!    link it prints is validated and stored as `Claiming(code, url)`;
//! 4. once the secret file exists the state is `Claimed`, and the agent's
//!    public address is read from the playit API (`rundata`); only an address
//!    that passes validation becomes `Running(address)`.
//!
//! Containment: the agent runs on the host, not in the game pod, and the
//! playit tunnel must point at `127.0.0.1:<mc_port>`; a tunnel that forwards
//! anywhere else is reported as an error and its address is not shown. The
//! game pod and its egress rules are not touched.
//!
//! Everything that touches the outside world is behind a trait
//! ([`HttpFetch`]) or a file path, so the tests run the whole thing against
//! fake agent scripts and canned HTTP answers; no test contacts GitHub or
//! playit.

mod backoff;
mod fetch;
mod install;
mod process;
mod release;
mod rundata;
mod secret;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use craft_a_server_core::tunnel::{
    TunnelEvent, TunnelInfo, TunnelState, clean_reason, next as next_state,
};
use rand::RngExt;
use tokio::process::Child;
use tokio::sync::{Mutex, Notify, watch};

use crate::db::{Db, DbError, TunnelRow};

pub use backoff::{Backoff, delay_for};
pub use fetch::{
    DownloadError, FetchError, FetchMeta, FetchMethod, FetchRequest, HttpFetch, UreqFetch,
};
pub use install::{AgentPaths, InstallError, ensure_installed, sha256_hex, verify_paths};
pub use process::{TunnelDirs, extract_claim_url, redact};
pub use release::{AGENT_VERSION, PLACEHOLDER_SHA256, Platform, Release};
pub use rundata::{RUNDATA_URL, Rundata, parse_rundata};
pub use secret::{Secret, read_secret, secret_present};

#[cfg(test)]
pub(crate) use fetch::fake::FakeFetch;

use process::Tail;
use rundata::MAX_RUNDATA_BYTES;

/// Why a tunnel request was refused.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("online play needs an explicit confirmation of what is shared with playit.gg")]
    NotConfirmed,
    #[error("online play is not available on this kind of computer")]
    Unsupported,
    #[error("online play is not set up in this daemon")]
    NotAvailable,
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("i/o error while changing online play: {0}")]
    Io(#[from] std::io::Error),
}

/// How often things happen. Tests shrink these to milliseconds.
#[derive(Debug, Clone)]
pub struct Timings {
    /// Pause between supervisor passes.
    pub tick: Duration,
    /// First restart delay; doubles per consecutive failure.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// An agent that ran this long counts as healthy (backoff resets).
    pub stable_after: Duration,
    /// An agent that is still up after this long clears a crash error.
    pub healthy_after: Duration,
    /// Address lookups while there is no address yet.
    pub rundata_pending: Duration,
    /// Address lookups once running (to notice a deleted tunnel).
    pub rundata_running: Duration,
    /// How long the agent may take to open its control socket.
    pub socket_wait: Duration,
    /// After the claim program exits successfully, how long to wait for the
    /// secret to appear.
    pub claim_grace: Duration,
    /// A claim link is replaced by a fresh one after this long.
    pub claim_lifetime: Duration,
    /// Time a child gets to stop after SIGTERM before it is killed.
    pub stop_grace: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            tick: Duration::from_secs(2),
            backoff_base: Duration::from_secs(5),
            backoff_cap: Duration::from_secs(300),
            stable_after: Duration::from_secs(300),
            healthy_after: Duration::from_secs(5),
            rundata_pending: Duration::from_secs(15),
            rundata_running: Duration::from_secs(60),
            socket_wait: Duration::from_secs(30),
            claim_grace: Duration::from_secs(15),
            claim_lifetime: Duration::from_secs(20 * 60),
            stop_grace: Duration::from_secs(5),
        }
    }
}

/// Everything the manager needs to know about its surroundings.
#[derive(Debug, Clone)]
pub struct TunnelSettings {
    pub dirs: TunnelDirs,
    /// The Minecraft port the playit tunnel must forward to (on 127.0.0.1).
    pub mc_port: u16,
    pub release: Release,
    pub platform: Option<Platform>,
    pub rundata_url: String,
    pub timings: Timings,
}

impl TunnelSettings {
    /// Production settings: the pinned release, this machine's platform.
    pub fn production(root: &std::path::Path, mc_port: u16) -> TunnelSettings {
        TunnelSettings {
            dirs: TunnelDirs::new(root),
            mc_port,
            release: Release::pinned(),
            platform: Platform::detect(),
            rundata_url: RUNDATA_URL.to_owned(),
            timings: Timings::default(),
        }
    }
}

struct DaemonRun {
    child: Child,
    started: Instant,
    tail: Tail,
    /// Reads the agent's stderr into `tail`; awaited briefly after an exit
    /// so the last words are not lost.
    pump: Option<tokio::task::JoinHandle<()>>,
}

struct ClaimRun {
    child: Child,
    started: Instant,
    link: Arc<StdMutex<Option<(String, String)>>>,
}

#[derive(Default)]
struct Children {
    daemon: Option<DaemonRun>,
    claim: Option<ClaimRun>,
}

struct InstallFailure {
    permanent: bool,
    retry_at: Instant,
}

/// State only the supervisor pass touches (guarded by the tick lock).
struct Supervisor {
    reaped: bool,
    /// The last failure was the agent dying; cleared once a restart stays up.
    agent_failed: bool,
    paths: Option<AgentPaths>,
    install_failure: Option<InstallFailure>,
    install_backoff: Backoff,
    spawn_backoff: Backoff,
    next_spawn_at: Option<Instant>,
    claim_backoff: Backoff,
    next_claim_at: Option<Instant>,
    claim_exited_ok_at: Option<Instant>,
    last_rundata: Option<Instant>,
}

impl Supervisor {
    fn new(t: &Timings) -> Supervisor {
        Supervisor {
            reaped: false,
            agent_failed: false,
            paths: None,
            install_failure: None,
            install_backoff: Backoff::new(t.backoff_base, t.backoff_cap),
            spawn_backoff: Backoff::new(t.backoff_base, t.backoff_cap),
            next_spawn_at: None,
            claim_backoff: Backoff::new(t.backoff_base, t.backoff_cap),
            next_claim_at: None,
            claim_exited_ok_at: None,
            last_rundata: None,
        }
    }

    /// Forgets everything about running agents (used when switched off).
    fn reset_runtime(&mut self, t: &Timings) {
        let reaped = self.reaped;
        *self = Supervisor::new(t);
        self.reaped = reaped;
    }
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The tunnel service. Cheap to share behind an `Arc`.
pub struct TunnelManager {
    settings: TunnelSettings,
    db: Db,
    fetch: Arc<dyn HttpFetch>,
    now: Clock,
    /// Serializes read-modify-write of the stored state.
    gate: StdMutex<()>,
    /// Held for the whole of a supervisor pass.
    sup: Mutex<Supervisor>,
    /// Held only for short process operations.
    children: Mutex<Children>,
    /// Bumped by enable/disable so a pass in flight drops stale results.
    generation: AtomicU64,
    wake: Notify,
}

impl std::fmt::Debug for TunnelManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelManager")
            .field("mc_port", &self.settings.mc_port)
            .finish_non_exhaustive()
    }
}

fn jitter() -> f64 {
    rand::rng().random_range(0.0..=1.0)
}

fn describe_exit(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "stopped by a signal".to_owned(),
    }
}

impl TunnelManager {
    pub fn new(settings: TunnelSettings, db: Db, fetch: Arc<dyn HttpFetch>, now: Clock) -> Self {
        let sup = Supervisor::new(&settings.timings);
        TunnelManager {
            settings,
            db,
            fetch,
            now,
            gate: StdMutex::new(()),
            sup: Mutex::new(sup),
            children: Mutex::new(Children::default()),
            generation: AtomicU64::new(0),
            wake: Notify::new(),
        }
    }

    fn lock_gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn info_from(&self, row: TunnelRow) -> TunnelInfo {
        TunnelInfo {
            enabled: row.enabled,
            state: row.state,
            consent_unix: row.consent_unix,
            updated_unix: row.updated_unix,
            agent_version: self.settings.release.version.clone(),
            local_port: self.settings.mc_port,
        }
    }

    /// Current state for clients.
    pub fn info(&self) -> Result<TunnelInfo, TunnelError> {
        Ok(self.info_from(self.db.get_tunnel()?))
    }

    fn record(&self, kind: &str, detail: &str) {
        if let Err(e) = self.db.record_event((self.now)(), None, kind, detail) {
            tracing::warn!(error = %e, kind, "could not record tunnel event");
        }
    }

    /// Turns online play on. `confirm` must be true: the caller has shown
    /// the disclosure and the user agreed. Returns at once; the supervisor
    /// does the work.
    pub fn enable(&self, confirm: bool) -> Result<TunnelInfo, TunnelError> {
        if !confirm {
            return Err(TunnelError::NotConfirmed);
        }
        if self.settings.platform.is_none() {
            return Err(TunnelError::Unsupported);
        }
        let info = {
            let _gate = self.lock_gate();
            let row = self.db.get_tunnel()?;
            let has_secret = secret_present(&self.settings.dirs.secret_path);
            let state = next_state(&row.state, &TunnelEvent::Enabled { has_secret })
                .unwrap_or_else(|_| row.state.clone());
            let now = (self.now)();
            let new = TunnelRow {
                enabled: true,
                state,
                consent_unix: Some(now),
                updated_unix: now,
            };
            self.db.set_tunnel(&new)?;
            self.info_from(new)
        };
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.record(
            "tunnel_enabled",
            "online play was turned on after the disclosure",
        );
        self.wake.notify_one();
        Ok(info)
    }

    /// Turns online play off and stops the agent. With `forget_claim` the
    /// saved link to the playit.gg account is deleted too, so the next
    /// enable starts a fresh claim.
    pub async fn disable(&self, forget_claim: bool) -> Result<TunnelInfo, TunnelError> {
        let info = {
            let _gate = self.lock_gate();
            let row = self.db.get_tunnel()?;
            let has_secret = !forget_claim && secret_present(&self.settings.dirs.secret_path);
            let state = next_state(&row.state, &TunnelEvent::Disabled { has_secret })
                .unwrap_or(TunnelState::NotConfigured);
            let new = TunnelRow {
                enabled: false,
                state,
                consent_unix: row.consent_unix,
                updated_unix: (self.now)(),
            };
            self.db.set_tunnel(&new)?;
            self.info_from(new)
        };
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.stop_children().await;
        if forget_claim {
            let dirs = &self.settings.dirs;
            for path in [&dirs.secret_path, &dirs.socket_path] {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.record(
            "tunnel_disabled",
            if forget_claim {
                "online play was turned off and the playit.gg link was forgotten"
            } else {
                "online play was turned off"
            },
        );
        Ok(info)
    }

    /// Stops both child processes (the agent and the claim program).
    pub async fn stop_children(&self) {
        let (daemon, claim) = {
            let mut c = self.children.lock().await;
            (c.daemon.take(), c.claim.take())
        };
        let grace = self.settings.timings.stop_grace;
        if let Some(mut run) = claim {
            process::terminate(&mut run.child, grace).await;
        }
        if let Some(mut run) = daemon {
            process::terminate(&mut run.child, grace).await;
            process::remove_pid(&self.settings.dirs.pid_path);
        }
    }

    /// Stops everything. Called when the daemon shuts down.
    pub async fn shutdown(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.stop_children().await;
    }

    /// Runs the supervisor until `stop` becomes true, then shuts down.
    pub async fn run(&self, mut stop: watch::Receiver<bool>) {
        loop {
            self.tick().await;
            tokio::select! {
                () = tokio::time::sleep(self.settings.timings.tick) => {}
                () = self.wake.notified() => {}
                _ = stop.wait_for(|v| *v) => break,
            }
        }
        self.shutdown().await;
    }

    // ------------------------------------------------------------ state

    /// Applies an event to the stored state. Events other than enable and
    /// disable are ignored while the tunnel is off, so a late result cannot
    /// bring a disabled tunnel back to life.
    fn apply(&self, event: &TunnelEvent) {
        let _gate = self.lock_gate();
        let Ok(row) = self.db.get_tunnel() else {
            return;
        };
        if !row.enabled {
            return;
        }
        let state = match next_state(&row.state, event) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "tunnel event ignored");
                return;
            }
        };
        if state == row.state {
            return;
        }
        let kind_changed = state.name() != row.state.name();
        let new = TunnelRow {
            state: state.clone(),
            updated_unix: (self.now)(),
            ..row
        };
        if let Err(e) = self.db.set_tunnel(&new) {
            tracing::warn!(error = %e, "could not store the tunnel state");
            return;
        }
        if kind_changed {
            match &state {
                TunnelState::Error { reason } => {
                    tracing::warn!(reason = %reason, "online play error");
                    self.record("tunnel_error", reason);
                }
                other => {
                    tracing::info!(state = other.name(), "online play state changed");
                    self.record("tunnel_state", other.name());
                }
            }
        }
    }

    fn fail(&self, reason: &str) {
        self.apply(&TunnelEvent::Failed {
            reason: clean_reason(&process::clean_line(reason)),
        });
    }

    fn has_secret(&self) -> bool {
        secret_present(&self.settings.dirs.secret_path)
    }

    /// After a successful step, an `Error` state goes back to a resting one.
    fn recover(&self) {
        let has_secret = self.has_secret();
        self.apply(&TunnelEvent::Recovered { has_secret });
    }

    fn current_state(&self) -> Option<TunnelRow> {
        self.db.get_tunnel().ok()
    }

    // ------------------------------------------------------------- tick

    /// One supervisor pass. Safe to call at any time; does nothing useful
    /// (and stops children) when the tunnel is off.
    pub async fn tick(&self) {
        let mut sup = self.sup.lock().await;
        let timings = self.settings.timings.clone();
        let generation = self.generation.load(Ordering::SeqCst);

        if !sup.reaped {
            sup.reaped = true;
            let exe = self
                .settings
                .platform
                .map(|p| {
                    self.settings
                        .dirs
                        .bin
                        .join(&self.settings.release.version)
                        .join(p.daemon_asset())
                })
                .unwrap_or_default();
            if process::reap_stale(&self.settings.dirs.pid_path, &exe).await {
                tracing::warn!("stopped a playit agent left over from a previous run");
            }
        }

        let Some(row) = self.current_state() else {
            return;
        };
        if !row.enabled {
            self.stop_children().await;
            sup.reset_runtime(&timings);
            return;
        }
        let now = Instant::now();

        // 1. The agent files.
        if sup.paths.is_none() {
            if let Some(f) = &sup.install_failure
                && (f.permanent || now < f.retry_at)
            {
                return;
            }
            let (fetch, release, platform, bin) = (
                Arc::clone(&self.fetch),
                self.settings.release.clone(),
                self.settings.platform,
                self.settings.dirs.bin.clone(),
            );
            let result = tokio::task::spawn_blocking(move || {
                ensure_installed(fetch.as_ref(), &release, platform, &bin)
            })
            .await;
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            match result {
                Ok(Ok(paths)) => {
                    tracing::info!(version = %self.settings.release.version, "playit agent installed and verified");
                    sup.paths = Some(paths);
                    sup.install_failure = None;
                    sup.install_backoff.reset();
                    self.recover();
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "playit agent install failed");
                    let delay = sup.install_backoff.on_failure(jitter());
                    sup.install_failure = Some(InstallFailure {
                        permanent: e.is_permanent(),
                        retry_at: now + delay,
                    });
                    self.fail(&e.user_text());
                    return;
                }
                Err(e) => {
                    tracing::error!(error = %e, "install task failed");
                    return;
                }
            }
        }
        let Some(paths) = sup.paths.clone() else {
            return;
        };

        // 2. The agent process.
        if !self.supervise_daemon(&mut sup, &paths, generation).await {
            return;
        }

        // 3. Claim or address.
        if self.has_secret() {
            self.finish_claim(&mut sup).await;
            self.lookup_address(&mut sup, generation).await;
        } else {
            self.drive_claim(&mut sup, &paths, generation).await;
        }
    }

    /// Keeps the agent alive. Returns false when the rest of the pass should
    /// be skipped (no agent running right now).
    async fn supervise_daemon(
        &self,
        sup: &mut Supervisor,
        paths: &AgentPaths,
        generation: u64,
    ) -> bool {
        let timings = &self.settings.timings;
        let now = Instant::now();
        let exited = {
            let mut c = self.children.lock().await;
            match c.daemon.as_mut() {
                Some(run) => match run.child.try_wait() {
                    Ok(Some(status)) => {
                        let tail = run.tail.clone();
                        let pump = run.pump.take();
                        c.daemon = None;
                        Some((status, tail, pump))
                    }
                    Ok(None) => {
                        let up = run.started.elapsed();
                        if up >= timings.stable_after {
                            sup.spawn_backoff.reset();
                        }
                        if up >= timings.healthy_after && sup.agent_failed {
                            sup.agent_failed = false;
                            self.recover();
                        }
                        return true;
                    }
                    Err(_) => {
                        c.daemon = None;
                        None
                    }
                },
                None => None,
            }
        };
        if let Some((status, tail, pump)) = exited {
            if let Some(pump) = pump {
                let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
            }
            let last = tail.last();
            sup.agent_failed = true;
            process::remove_pid(&self.settings.dirs.pid_path);
            // The claim program is useless without its agent.
            let claim = self.children.lock().await.claim.take();
            if let Some(mut run) = claim {
                process::terminate(&mut run.child, timings.stop_grace).await;
            }
            let delay = sup.spawn_backoff.on_failure(jitter());
            sup.next_spawn_at = Some(now + delay);
            let mut reason = format!("the playit agent stopped ({})", describe_exit(status));
            if let Some(line) = last {
                reason.push_str(": ");
                reason.push_str(&line);
            }
            self.fail(&reason);
            return false;
        }
        // No agent: start one when the backoff allows it.
        if sup.next_spawn_at.is_some_and(|t| now < t) {
            return false;
        }
        if let Err(e) = verify_paths(&self.settings.release, self.settings.platform, paths) {
            // A modified or missing file is never run; reinstall from the pin.
            tracing::error!(error = %e, "installed playit agent failed verification");
            sup.paths = None;
            sup.install_failure = None;
            self.fail(&e.user_text());
            return false;
        }
        if self.generation.load(Ordering::SeqCst) != generation {
            return false;
        }
        match process::spawn_daemon(paths, &self.settings.dirs) {
            Ok(mut child) => {
                let tail = Tail::default();
                if let Some(pid) = child.id() {
                    process::write_pid(&self.settings.dirs.pid_path, pid);
                    tracing::info!(pid, "playit agent started");
                }
                let pump = child.stderr.take().map(|stderr| {
                    let tail = tail.clone();
                    tokio::spawn(process::pump_lines(stderr, move |l| tail.push(&l)))
                });
                self.children.lock().await.daemon = Some(DaemonRun {
                    child,
                    started: Instant::now(),
                    tail,
                    pump,
                });
                sup.next_spawn_at = None;
                true
            }
            Err(e) => {
                tracing::error!(error = %e, "could not start the playit agent");
                sup.agent_failed = true;
                let delay = sup.spawn_backoff.on_failure(jitter());
                sup.next_spawn_at = Some(now + delay);
                self.fail("the playit agent could not be started");
                false
            }
        }
    }

    /// No secret yet: run the claim program and publish its link.
    async fn drive_claim(&self, sup: &mut Supervisor, paths: &AgentPaths, generation: u64) {
        enum View {
            None,
            Alive(Option<(String, String)>),
            Exited { success: bool },
            Expired(Option<Box<ClaimRun>>),
        }

        let timings = &self.settings.timings;
        let now = Instant::now();
        let dirs = &self.settings.dirs;

        // A resting state with no secret behind it (the file was removed).
        if let Some(row) = self.current_state()
            && matches!(
                row.state,
                TunnelState::Claimed | TunnelState::Running { .. }
            )
        {
            self.fail("the saved link to playit.gg is gone; linking again");
        }

        let view = {
            let mut c = self.children.lock().await;
            match c.claim.as_mut() {
                None => View::None,
                Some(run) => match run.child.try_wait() {
                    Ok(None) => {
                        if run.started.elapsed() > timings.claim_lifetime {
                            View::Expired(c.claim.take().map(Box::new))
                        } else {
                            View::Alive(run.link.lock().unwrap_or_else(|e| e.into_inner()).clone())
                        }
                    }
                    Ok(Some(status)) => {
                        c.claim = None;
                        View::Exited {
                            success: status.success(),
                        }
                    }
                    Err(_) => {
                        c.claim = None;
                        View::Exited { success: false }
                    }
                },
            }
        };
        match view {
            View::Alive(link) => {
                if let Some((code, url)) = link {
                    self.apply(&TunnelEvent::ClaimLinkReady { code, url });
                }
                return;
            }
            View::Exited { success: true } => {
                // Exited cleanly: the agent should be writing the secret.
                sup.claim_exited_ok_at = Some(now);
                return;
            }
            View::Exited { success: false } => {
                let delay = sup.claim_backoff.on_failure(jitter());
                sup.next_claim_at = Some(now + delay);
                self.fail("the link to playit.gg was not completed; a new link will be made");
                return;
            }
            View::Expired(old) => {
                // The old link is stale; a fresh one follows right away.
                if let Some(mut old) = old {
                    process::terminate(&mut old.child, timings.stop_grace).await;
                }
                sup.next_claim_at = None;
            }
            View::None => {}
        }
        if let Some(since) = sup.claim_exited_ok_at {
            if now.duration_since(since) < timings.claim_grace {
                return;
            }
            sup.claim_exited_ok_at = None;
            let delay = sup.claim_backoff.on_failure(jitter());
            sup.next_claim_at = Some(now + delay);
            self.fail("the link to playit.gg was not completed; a new link will be made");
            return;
        }

        // Start a claim program once the agent's socket exists.
        if sup.next_claim_at.is_some_and(|t| now < t) {
            return;
        }
        if std::fs::symlink_metadata(&dirs.socket_path).is_err() {
            let waited = {
                let c = self.children.lock().await;
                c.daemon.as_ref().map(|d| d.started.elapsed())
            };
            if waited.is_some_and(|w| w > timings.socket_wait) {
                self.fail("the playit agent did not open its control socket");
            }
            return;
        }
        if self.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        match process::spawn_claim_cli(paths, dirs) {
            Ok(mut child) => {
                let link: Arc<StdMutex<Option<(String, String)>>> = Arc::default();
                if let Some(stdout) = child.stdout.take() {
                    let link = Arc::clone(&link);
                    tokio::spawn(process::pump_lines(stdout, move |line| {
                        if let Some(found) = extract_claim_url(&line) {
                            *link.lock().unwrap_or_else(|e| e.into_inner()) = Some(found);
                        }
                    }));
                }
                self.children.lock().await.claim = Some(ClaimRun {
                    child,
                    started: Instant::now(),
                    link,
                });
                tracing::info!("playit claim program started");
            }
            Err(e) => {
                tracing::error!(error = %e, "could not start the playit claim program");
                let delay = sup.claim_backoff.on_failure(jitter());
                sup.next_claim_at = Some(now + delay);
                self.fail("the playit claim program could not be started");
            }
        }
    }

    /// The secret exists: the claim program is done.
    async fn finish_claim(&self, sup: &mut Supervisor) {
        let claim = self.children.lock().await.claim.take();
        if let Some(mut run) = claim {
            process::terminate(&mut run.child, self.settings.timings.stop_grace).await;
        }
        sup.claim_exited_ok_at = None;
        sup.claim_backoff.reset();
        // Only a claim in progress becomes `Claimed`; an error stays visible
        // until what caused it is gone.
        if matches!(
            self.current_state().map(|r| r.state),
            Some(TunnelState::Claiming { .. } | TunnelState::NotConfigured)
        ) {
            self.apply(&TunnelEvent::SecretStored);
        }
    }

    /// Asks the playit API for the agent's tunnels and publishes the address.
    async fn lookup_address(&self, sup: &mut Supervisor, generation: u64) {
        let timings = &self.settings.timings;
        let running = matches!(
            self.current_state().map(|r| r.state),
            Some(TunnelState::Running { .. })
        );
        let every = if running {
            timings.rundata_running
        } else {
            timings.rundata_pending
        };
        let now = Instant::now();
        if sup
            .last_rundata
            .is_some_and(|t| now.duration_since(t) < every)
        {
            return;
        }
        sup.last_rundata = Some(now);

        let secret = match read_secret(&self.settings.dirs.secret_path) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "cannot use the agent's secret file");
                self.fail("the saved playit.gg link is unreadable; turn online play off, unlink and link again");
                return;
            }
        };
        let request = FetchRequest {
            method: FetchMethod::Post,
            url: self.settings.rundata_url.clone(),
            authorization: Some(secret.authorization()),
            body: Some(b"{}".to_vec()),
            max_bytes: MAX_RUNDATA_BYTES,
        };
        let fetch = Arc::clone(&self.fetch);
        let answer = tokio::task::spawn_blocking(move || {
            let mut body = Vec::new();
            fetch.fetch(&request, &mut body).map(|meta| (meta, body))
        })
        .await;
        if self.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let (meta, body) = match answer {
            Ok(Ok(ok)) => ok,
            Ok(Err(e)) => {
                // playit.gg unreachable or slow: not our fault, nothing to change.
                tracing::debug!(error = %e, "address lookup failed");
                return;
            }
            Err(_) => return,
        };
        match meta.status {
            200 => match parse_rundata(&body, self.settings.mc_port) {
                Ok(Rundata::Address(address)) => {
                    self.apply(&TunnelEvent::AddressKnown { address });
                }
                Ok(Rundata::NoTunnel) => {
                    // A stale error (an unsafe tunnel that was since removed)
                    // is over; the claim itself is fine.
                    self.recover();
                    self.apply(&TunnelEvent::AddressLost);
                }
                Ok(Rundata::UnsafeOrigin) => self.fail(&format!(
                    "your playit tunnel forwards to an address other than this machine; \
                     change its destination to 127.0.0.1:{} in the playit dashboard",
                    self.settings.mc_port
                )),
                Err(e) => self.fail(&format!("{e}; this version may need an update")),
            },
            401 | 403 => self.fail(
                "playit.gg rejected the saved link; turn online play off, unlink and link again",
            ),
            status => tracing::debug!(status, "address lookup answered with an error status"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
