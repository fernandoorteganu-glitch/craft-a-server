//! Creating, starting, stopping and resetting servers.

use std::time::{Duration, Instant};

use craft_a_server_core::memory;
use craft_a_server_core::model::{ServerEvent, ServerState};
use rand::RngExt;

use super::{Result, Service, ServiceError, StatusProbe};
use crate::db::ServerRow;
use crate::runtime::{ContainerRuntime, ContainerState, ServerContainerSpec};

const NAME_MAX_CHARS: usize = 48;
const RCON_PASSWORD_LEN: usize = 32;

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    /// Creates a stopped server. The EULA must be accepted explicitly.
    pub async fn create_server(
        &self,
        name: &str,
        mc_version: &str,
        eula_accepted: bool,
    ) -> Result<ServerRow> {
        if !eula_accepted {
            return Err(ServiceError::EulaNotAccepted);
        }
        let name = validate_name(name)?;
        validate_mc_version(mc_version)?;

        let id = new_server_id();
        let _guard = self.lock(&id).await;
        let port = self.allocate_port()?;
        let row = ServerRow {
            id: id.clone(),
            name,
            mc_version: mc_version.to_owned(),
            state: ServerState::Creating,
            host_port: port,
            memory_max_mib: None,
            rcon_password: new_rcon_password(),
            eula_accepted_at: self.now(),
            created_unix: self.now(),
        };

        let data = self.data_dir(&id);
        std::fs::create_dir_all(&data).map_err(ServiceError::io("creating the data directory"))?;
        restrict_dir(&self.server_dir(&id))?;
        restrict_dir(&data)?;
        if let Err(e) = self.db.insert_server(&row) {
            let _ = std::fs::remove_dir_all(self.server_dir(&id));
            return Err(e.into());
        }
        self.event(
            Some(&id),
            "created",
            &format!("{} ({mc_version})", row.name),
        );
        let state = self.transition(&row, ServerEvent::Created)?;
        Ok(ServerRow { state, ..row })
    }

    /// Lowest port from the configured base that no server uses.
    fn allocate_port(&self) -> Result<u16> {
        let used: Vec<u16> = self
            .db
            .list_servers()?
            .into_iter()
            .map(|s| s.host_port)
            .collect();
        (self.cfg.mc_port..=u16::MAX)
            .find(|p| !used.contains(p))
            .ok_or_else(|| ServiceError::InvalidInput("no free port left".into()))
    }

    /// Starts a stopped server and waits until it answers the status ping.
    pub async fn start(&self, id: &str) -> Result<()> {
        let _guard = self.lock(id).await;
        // An explicit start is a fresh attempt; the crash supervisor's own
        // restarts keep counting so a crash loop cannot reset itself.
        self.clear_failures(id);
        self.start_locked(id).await
    }

    pub(crate) async fn start_locked(&self, id: &str) -> Result<()> {
        let row = self.get_row(id)?;
        let plan = memory::plan(
            self.env.total_ram_bytes,
            self.cfg.memory_reserve_mib,
            row.memory_max_mib.or(self.cfg.memory_max_mib),
        )?;
        // Checks the transition is legal before anything is touched.
        let starting = self.transition(&row, ServerEvent::StartRequested)?;
        let row = ServerRow {
            state: starting,
            ..row
        };

        let spec = self.build_spec(&row, &plan);
        if let Err(e) = self.rt.remove(id).await {
            return self
                .fail_start(&row, format!("could not clear old containers: {e}"))
                .await;
        }
        if let Err(e) = self.rt.create_and_start(&spec).await {
            return self.fail_start(&row, e.to_string()).await;
        }
        match self.wait_ready(&row).await {
            Ok(()) => {
                self.transition(&row, ServerEvent::PingOk)?;
                Ok(())
            }
            Err(reason) => self.fail_start(&row, reason).await,
        }
    }

    fn build_spec(&self, row: &ServerRow, plan: &memory::MemoryPlan) -> ServerContainerSpec {
        let cpus = if self.cfg.cpus == 0 {
            std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
        } else {
            self.cfg.cpus
        };
        ServerContainerSpec {
            id: row.id.clone(),
            mc_version: row.mc_version.clone(),
            image: self.cfg.image.clone(),
            data_dir: self.data_dir(&row.id),
            host_port: row.host_port,
            memory_limit_mib: u32::try_from(plan.container_limit_mib).unwrap_or(u32::MAX),
            heap_mib: u32::try_from(plan.heap_mib).unwrap_or(u32::MAX),
            cpus: f64::from(cpus),
            pids_limit: self.cfg.pids_limit,
            uid: self.env.uid,
            gid: self.env.gid,
            stop_timeout_secs: u32::try_from(self.cfg.stop_timeout_secs).unwrap_or(u32::MAX),
            eula_accepted: true,
            rcon_password: row.rcon_password.clone(),
        }
    }

    /// Polls until the server answers the ping, the container dies, or time runs out.
    ///
    /// A slow start is not a failure: the deadline only applies while the
    /// container is still alive, and a dead container fails immediately.
    async fn wait_ready(&self, row: &ServerRow) -> std::result::Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(self.cfg.start_timeout_secs);
        loop {
            if self.probe.status(row.host_port).await.is_some() {
                return Ok(());
            }
            match self.rt.inspect(&row.id).await {
                Ok(info) => match info.state {
                    ContainerState::Running => {}
                    ContainerState::Exited { code } => {
                        let tail = self.log_tail_summary(&row.id).await;
                        let oom = if info.oom_killed {
                            " (out of memory)"
                        } else {
                            ""
                        };
                        return Err(format!("server exited with code {code}{oom}{tail}"));
                    }
                    ContainerState::Missing => return Err("server container disappeared".into()),
                    ContainerState::Other(_) => {}
                },
                Err(e) => return Err(format!("cannot inspect the server: {e}")),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "server did not answer within {} seconds",
                    self.cfg.start_timeout_secs
                ));
            }
            tokio::time::sleep(self.env.poll_interval).await;
        }
    }

    async fn log_tail_summary(&self, id: &str) -> String {
        match self.rt.logs_tail(id, 5).await {
            Ok(lines) if !lines.is_empty() => format!(": {}", lines.join(" | ")),
            _ => String::new(),
        }
    }

    /// Cleans up after a failed start and marks the server failed.
    async fn fail_start(&self, row: &ServerRow, reason: String) -> Result<()> {
        if let Err(e) = self.rt.remove(&row.id).await {
            tracing::warn!(server = %row.id, error = %e, "cleanup after failed start");
        }
        self.bump_failures(&row.id);
        self.transition(row, ServerEvent::Failure(reason.clone()))?;
        Err(ServiceError::StartFailed(reason))
    }

    /// Stops a server cleanly: save, `stop`, wait, and only then force.
    pub async fn stop(&self, id: &str) -> Result<()> {
        let _guard = self.lock(id).await;
        self.stop_locked(id).await
    }

    pub(crate) async fn stop_locked(&self, id: &str) -> Result<()> {
        let row = self.get_row(id)?;
        if row.state == ServerState::Stopped {
            return Ok(());
        }
        let stopping = self.transition(&row, ServerEvent::StopRequested)?;
        let row = ServerRow {
            state: stopping,
            ..row
        };

        // Best effort: if RCON is unreachable the SIGTERM below still saves.
        for command in ["save-all flush", "stop"] {
            if let Err(e) = self.rt.rcon(id, command).await {
                tracing::debug!(server = %id, command, error = %e, "rcon before stop");
                break;
            }
        }
        let grace = Duration::from_secs(self.cfg.stop_timeout_secs);
        if let Err(e) = self.rt.stop(id, grace).await {
            tracing::warn!(server = %id, error = %e, "graceful stop failed, killing");
            if let Err(e) = self.rt.kill(id).await {
                tracing::warn!(server = %id, error = %e, "kill failed");
            }
        }
        if let Err(e) = self.rt.remove(id).await {
            self.transition(
                &row,
                ServerEvent::Failure(format!("could not remove containers: {e}")),
            )?;
            return Err(e.into());
        }
        self.transition(&row, ServerEvent::Exited)?;
        Ok(())
    }

    /// Stops (if needed) and starts again.
    pub async fn restart(&self, id: &str) -> Result<()> {
        let _guard = self.lock(id).await;
        self.clear_failures(id);
        self.stop_locked(id).await?;
        self.start_locked(id).await
    }

    /// Acknowledges a failed server so it can be started again.
    pub async fn reset(&self, id: &str) -> Result<()> {
        let _guard = self.lock(id).await;
        let row = self.get_row(id)?;
        self.rt.remove(id).await?;
        self.transition(&row, ServerEvent::Reset)?;
        self.clear_failures(id);
        Ok(())
    }

    /// Deletes a stopped server, its data and its backups.
    pub async fn delete_server(&self, id: &str) -> Result<()> {
        let _guard = self.lock(id).await;
        let row = self.get_row(id)?;
        if !matches!(row.state, ServerState::Stopped | ServerState::Failed(_)) {
            return Err(ServiceError::WrongState {
                id: id.to_owned(),
                state: row.state.to_parts().0.to_owned(),
            });
        }
        self.rt.remove(id).await?;
        self.db.delete_server(id)?;
        self.event(Some(id), "deleted", &row.name);
        std::fs::remove_dir_all(self.server_dir(id)).or_else(ignore_not_found("removing data"))?;
        std::fs::remove_dir_all(self.server_backups_dir(id))
            .or_else(ignore_not_found("removing backups"))?;
        Ok(())
    }

    /// Runs one whitelisted console command: `whitelist add|remove <name>` or `kick <name>`.
    pub async fn console(&self, id: &str, command: &Console) -> Result<String> {
        let _guard = self.lock(id).await;
        let row = self.get_row(id)?;
        if row.state != ServerState::Running {
            return Err(ServiceError::WrongState {
                id: id.to_owned(),
                state: row.state.to_parts().0.to_owned(),
            });
        }
        Ok(self.rt.rcon(id, &command.to_rcon()?).await?)
    }
}

/// The closed set of console commands the panel may send (no free text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Console {
    WhitelistAdd(String),
    WhitelistRemove(String),
    Kick(String),
    WhitelistList,
}

impl Console {
    pub fn to_rcon(&self) -> Result<String> {
        Ok(match self {
            Console::WhitelistAdd(n) => format!("whitelist add {}", player_name(n)?),
            Console::WhitelistRemove(n) => format!("whitelist remove {}", player_name(n)?),
            Console::Kick(n) => format!("kick {}", player_name(n)?),
            Console::WhitelistList => "whitelist list".to_owned(),
        })
    }
}

/// Java Edition names: 3-16 of `[A-Za-z0-9_]`. Anything else never reaches RCON.
pub fn player_name(name: &str) -> Result<&str> {
    let ok = (3..=16).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ok {
        Ok(name)
    } else {
        Err(ServiceError::InvalidInput(
            "player names are 3-16 letters, digits or underscores".into(),
        ))
    }
}

fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > NAME_MAX_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(ServiceError::InvalidInput(format!(
            "the server name must be 1-{NAME_MAX_CHARS} characters without control characters"
        )));
    }
    Ok(name.to_owned())
}

fn validate_mc_version(v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && v.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(ServiceError::InvalidInput(
            "invalid Minecraft version".into(),
        ))
    }
}

fn new_server_id() -> String {
    let n: u32 = rand::rng().random();
    format!("s{n:08x}")
}

fn new_rcon_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..RCON_PASSWORD_LEN)
        .map(|_| {
            let i = rng.random_range(0..ALPHABET.len());
            ALPHABET.get(i).copied().map_or('a', char::from)
        })
        .collect()
}

#[cfg(unix)]
fn restrict_dir(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(ServiceError::io("setting directory permissions"))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

pub(super) fn ignore_not_found(context: &'static str) -> impl Fn(std::io::Error) -> Result<()> {
    move |e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(ServiceError::Io { context, source: e })
        }
    }
}
