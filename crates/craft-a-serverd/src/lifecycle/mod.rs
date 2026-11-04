//! The lifecycle service: the only code that changes a server's state.
//!
//! Every operation on a server runs under that server's lock, persists each
//! state transition before acting on it, and records an audit event. The
//! container engine and the status probe are generic so the whole service is
//! tested against fakes.

mod backups;
mod mods;
mod performance;
mod recovery;
mod server;
mod tunnel;
mod world;

// The service targets Linux (its fake runtime validates Linux paths), so these
// run on unix only; on a Windows dev box use WSL.
#[cfg(all(test, unix))]
mod tests;

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_a_server_core::config::Config;
use craft_a_server_core::mc_protocol::ServerStatus;
use craft_a_server_core::memory::MemoryError;
use craft_a_server_core::model::{ServerEvent, ServerState};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::backup::BackupError;
use crate::db::{Db, DbError, ServerRow};
use crate::mods::{ModError, ModrinthClient};
use crate::runtime::{ContainerRuntime, RuntimeError};
use crate::tunnel::{TunnelError, TunnelManager};

pub use backups::Reason;
pub use mods::{ModBrowse, ModBrowseHit};
pub use performance::{GameUse, HostUse, Resources};
pub use recovery::ReconcileReport;
pub use server::{Console, player_name};
pub use world::WorldImportOutcome;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("no such server or backup: {0}")]
    NotFound(String),
    #[error("server '{id}' is {state}, which does not allow this")]
    WrongState { id: String, state: String },
    #[error("the Minecraft EULA must be accepted first")]
    EulaNotAccepted,
    #[error("server failed to start: {0}")]
    StartFailed(String),
    #[error("not enough free disk space for this operation")]
    DiskLow,
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Backup(#[from] BackupError),
    #[error(transparent)]
    Mods(#[from] ModError),
    #[error(transparent)]
    Tunnel(#[from] TunnelError),
    /// A world import that cannot be done; the text is written for people.
    #[error("{0}")]
    WorldImport(String),
    #[error("i/o error during {context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
}

pub type Result<T> = std::result::Result<T, ServiceError>;

impl ServiceError {
    fn io(context: &'static str) -> impl FnOnce(io::Error) -> ServiceError {
        move |source| ServiceError::Io { context, source }
    }
}

/// Answers "is the Minecraft server on this port up?".
pub trait StatusProbe: Send + Sync {
    fn status(&self, port: u16) -> impl Future<Output = Option<ServerStatus>> + Send;
}

/// Real probe: a Server List Ping to loopback.
#[derive(Debug, Default, Clone)]
pub struct TcpProbe;

impl StatusProbe for TcpProbe {
    async fn status(&self, port: u16) -> Option<ServerStatus> {
        crate::runtime::mc_status("127.0.0.1", port, Duration::from_secs(3))
            .await
            .ok()
    }
}

/// Free and total bytes of the filesystem holding a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
type DiskProbe = Arc<dyn Fn(&Path) -> io::Result<DiskUsage> + Send + Sync>;

/// Everything the service needs from the machine, injectable for tests.
#[derive(Clone)]
pub struct Env {
    pub total_ram_bytes: u64,
    pub uid: u32,
    pub gid: u32,
    pub now_unix: Clock,
    pub disk: DiskProbe,
    /// How often startup polls the status ping.
    pub poll_interval: Duration,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("total_ram_bytes", &self.total_ram_bytes)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl Env {
    /// The real machine: `/proc/meminfo`, `statvfs`, the current user.
    pub fn system() -> io::Result<Env> {
        Ok(Env {
            total_ram_bytes: read_total_ram()?,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            now_unix: Arc::new(system_now),
            disk: Arc::new(statvfs_usage),
            poll_interval: Duration::from_secs(2),
        })
    }
}

#[cfg(unix)]
fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(unix)]
fn read_total_ram() -> io::Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    parse_meminfo_total(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MemTotal not found"))
}

/// Extracts `MemTotal` (reported in kB) from `/proc/meminfo` text, in bytes.
pub fn parse_meminfo_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024)
}

#[cfg(unix)]
fn statvfs_usage(path: &Path) -> io::Result<DiskUsage> {
    let s = rustix::fs::statvfs(path)?;
    Ok(DiskUsage {
        free_bytes: s.f_bavail.saturating_mul(s.f_frsize),
        total_bytes: s.f_blocks.saturating_mul(s.f_frsize),
    })
}

/// The lifecycle service.
#[allow(missing_debug_implementations)] // holds a runtime and probe that need not be Debug
pub struct Service<R, P> {
    pub(crate) cfg: Arc<Config>,
    pub(crate) db: Db,
    pub(crate) rt: Arc<R>,
    pub(crate) probe: Arc<P>,
    pub(crate) env: Env,
    /// Present when mod management is set up (see [`Service::with_modrinth`]).
    pub(crate) modrinth: Option<Arc<ModrinthClient>>,
    /// Present when the list of game versions can be fetched.
    pub(crate) versions: Option<Arc<crate::versions::VersionCatalog>>,
    /// Present when online play (the playit.gg tunnel) is set up.
    pub(crate) tunnel: Option<Arc<TunnelManager>>,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    failures: Mutex<HashMap<String, u32>>,
    cpu_samples: Mutex<HashMap<String, performance::CpuSample>>,
}

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    pub fn new(cfg: Arc<Config>, db: Db, rt: Arc<R>, probe: Arc<P>, env: Env) -> Self {
        Service {
            cfg,
            db,
            rt,
            probe,
            env,
            modrinth: None,
            versions: None,
            tunnel: None,
            locks: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            cpu_samples: Mutex::new(HashMap::new()),
        }
    }

    /// Enables mod management with the given Modrinth client.
    #[must_use]
    pub fn with_modrinth(mut self, client: Arc<ModrinthClient>) -> Self {
        self.modrinth = Some(client);
        self
    }

    /// Enables the version picker's list of game versions.
    #[must_use]
    pub fn with_versions(mut self, catalog: Arc<crate::versions::VersionCatalog>) -> Self {
        self.versions = Some(catalog);
        self
    }

    /// The game versions to offer on the create screen, newest first. Falls
    /// back to a short built-in list; never fails.
    pub async fn game_versions(&self) -> Vec<crate::versions::GameVersion> {
        let Some(catalog) = self.versions.clone() else {
            return crate::versions::fallback();
        };
        tokio::task::spawn_blocking(move || catalog.list())
            .await
            .unwrap_or_else(|_| crate::versions::fallback())
    }

    /// Enables online play with the given tunnel manager.
    #[must_use]
    pub fn with_tunnel(mut self, tunnel: Arc<TunnelManager>) -> Self {
        self.tunnel = Some(tunnel);
        self
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub(crate) fn now(&self) -> i64 {
        (self.env.now_unix)()
    }

    /// Serializes operations on one server.
    pub(crate) async fn lock(&self, id: &str) -> OwnedMutexGuard<()> {
        let arc = {
            let mut map = self.locks.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(id.to_owned()).or_default().clone()
        };
        arc.lock_owned().await
    }

    pub(crate) fn bump_failures(&self, id: &str) -> u32 {
        let mut map = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.entry(id.to_owned()).or_insert(0);
        *n += 1;
        *n
    }

    pub(crate) fn clear_failures(&self, id: &str) {
        let mut map = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(id);
    }

    pub(crate) fn get_row(&self, id: &str) -> Result<ServerRow> {
        self.db
            .get_server(id)?
            .ok_or_else(|| ServiceError::NotFound(id.to_owned()))
    }

    pub fn get_server(&self, id: &str) -> Result<ServerRow> {
        self.get_row(id)
    }

    pub fn list_servers(&self) -> Result<Vec<ServerRow>> {
        Ok(self.db.list_servers()?)
    }

    /// Applies `event` to the stored state, persists it and logs it.
    pub(crate) fn transition(&self, row: &ServerRow, event: ServerEvent) -> Result<ServerState> {
        let next = row
            .state
            .next(event)
            .map_err(|_| ServiceError::WrongState {
                id: row.id.clone(),
                state: row.state.to_parts().0.to_owned(),
            })?;
        self.set_state(&row.id, &row.state, &next)?;
        Ok(next)
    }

    pub(crate) fn set_state(&self, id: &str, from: &ServerState, to: &ServerState) -> Result<()> {
        self.db.set_state(id, to)?;
        let detail = match to.to_parts() {
            (name, Some(reason)) => format!("{} -> {name}: {reason}", from.to_parts().0),
            (name, None) => format!("{} -> {name}", from.to_parts().0),
        };
        self.event(Some(id), "state", &detail);
        Ok(())
    }

    /// Audit-log entry; a failing log write must never break the operation.
    pub(crate) fn event(&self, server_id: Option<&str>, kind: &str, detail: &str) {
        if let Err(e) = self.db.record_event(self.now(), server_id, kind, detail) {
            tracing::warn!(error = %e, kind, "could not record event");
        }
    }

    pub(crate) fn server_dir(&self, id: &str) -> PathBuf {
        self.cfg.servers_dir().join(id)
    }

    pub(crate) fn data_dir(&self, id: &str) -> PathBuf {
        self.server_dir(id).join("data")
    }

    pub(crate) fn server_backups_dir(&self, id: &str) -> PathBuf {
        self.cfg.backups_dir().join(id)
    }
}

#[cfg(test)]
mod unit {
    use super::parse_meminfo_total;

    #[test]
    fn parses_meminfo() {
        let text = "MemTotal:       16383492 kB\nMemFree:  1 kB\n";
        assert_eq!(parse_meminfo_total(text), Some(16_383_492 * 1024));
        assert_eq!(parse_meminfo_total("nothing"), None);
        assert_eq!(parse_meminfo_total("MemTotal: abc kB"), None);
    }
}
