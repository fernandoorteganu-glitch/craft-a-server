//! Boot reconciliation and crash supervision.

use craft_a_server_core::model::{ServerEvent, ServerState};

use super::{Result, Service, StatusProbe};
use crate::backup::restore::{self, SwapRecovery};
use crate::runtime::{ContainerRuntime, ContainerState};

/// What reconciliation found and fixed, for the log and `diagnose`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub servers_checked: usize,
    pub swaps_recovered: Vec<String>,
    /// Servers marked stopped/failed because their container was gone.
    pub state_corrected: Vec<String>,
    /// Running servers whose autosave was re-enabled (an interrupted backup).
    pub autosave_restored: Vec<String>,
    /// Pods with our label but no matching server row (left alone).
    pub orphan_pods: Vec<String>,
    /// Servers that were meant to be running and must be started again.
    pub to_autostart: Vec<String>,
    /// Half-written backup files (`*.partial`) left by a crash, now deleted.
    pub partial_backups_removed: usize,
}

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    /// Brings the stored state in line with reality after a (re)start.
    ///
    /// Safe to run at every boot: it only corrects the database, finishes an
    /// interrupted data swap and re-enables autosave. It never deletes a world.
    pub async fn reconcile(&self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        let servers = self.db.list_servers()?;
        let pods = self.rt.list_managed().await?;

        for server in &servers {
            report.servers_checked += 1;
            let _guard = self.lock(&server.id).await;
            let server = self.get_row(&server.id)?;

            let live = self.data_dir(&server.id);
            let trash = self.server_dir(&server.id).join("data.old");
            match restore::recover_interrupted_swap(&live, &trash) {
                Ok(SwapRecovery::RestoredFromTrash) => {
                    self.event(
                        Some(&server.id),
                        "swap_recovered",
                        "restored data from data.old",
                    );
                    report.swaps_recovered.push(server.id.clone());
                }
                Ok(SwapRecovery::Nothing) => {}
                Err(e) => tracing::error!(server = %server.id, error = %e, "swap recovery failed"),
            }

            report.partial_backups_removed += self.remove_partial_backups(&server.id);

            let info = self.rt.inspect(&server.id).await?;
            let running = info.state == ContainerState::Running;
            match (&server.state, running) {
                (ServerState::Running, true) => {
                    // A backup interrupted by a daemon crash leaves autosave off.
                    if self.rt.rcon(&server.id, "save-on").await.is_ok() {
                        report.autosave_restored.push(server.id.clone());
                    }
                }
                (ServerState::Running | ServerState::Starting | ServerState::Stopping, false)
                    if server.auto_start =>
                {
                    // Power cut or reboot: the user wanted it running, so it is
                    // not a failure. Put it back to a clean stop and queue a start.
                    self.rt.remove(&server.id).await?;
                    self.set_state(&server.id, &server.state, &ServerState::Stopped)?;
                    self.event(Some(&server.id), "unclean_shutdown", "will start again");
                    report.to_autostart.push(server.id.clone());
                }
                (ServerState::Running | ServerState::Starting | ServerState::Stopping, false) => {
                    let reason =
                        "the server was not running after a restart of the system".to_owned();
                    let failed = ServerState::Failed(reason);
                    self.set_state(&server.id, &server.state, &failed)?;
                    // Leave no half-dead pod behind.
                    self.rt.remove(&server.id).await?;
                    report.state_corrected.push(server.id.clone());
                }
                (ServerState::Stopped, true) => {
                    // Containers outlived a stop we recorded: finish the job.
                    self.rt.remove(&server.id).await?;
                    report.state_corrected.push(server.id.clone());
                }
                _ => {}
            }
        }

        for pod in pods {
            if !servers.iter().any(|s| s.id == pod.server_id) {
                report.orphan_pods.push(pod.pod_name);
            }
        }
        self.event(None, "reconciled", &format!("{report:?}"));
        Ok(report)
    }

    /// Deletes `*.partial` files in a server's backup directory. A partial
    /// file is by construction never a valid backup (the final name appears
    /// only after fsync + rename), so removing it cannot lose data.
    fn remove_partial_backups(&self, id: &str) -> usize {
        let Ok(entries) = std::fs::read_dir(self.server_backups_dir(id)) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let is_partial = path.extension().is_some_and(|e| e == "partial");
            if is_partial && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Starts the servers `reconcile` queued, one at a time (each start can
    /// take a minute, and starting them all at once would fight over RAM).
    pub async fn autostart(&self, ids: &[String]) {
        for id in ids {
            match self.start(id).await {
                Ok(()) => tracing::info!(server = %id, "started again after reboot"),
                Err(e) => tracing::error!(server = %id, error = %e, "auto-start failed"),
            }
        }
    }

    /// Looks at every running server once; call it periodically.
    ///
    /// A server that died is restarted unless it already failed
    /// `max_consecutive_failures` times in a row, in which case it stays
    /// `failed` with its reason and nobody retries behind the user's back.
    pub async fn supervise_once(&self) -> Result<()> {
        for server in self.db.list_servers()? {
            if server.state != ServerState::Running {
                continue;
            }
            let _guard = self.lock(&server.id).await;
            let row = self.get_row(&server.id)?;
            if row.state != ServerState::Running {
                continue;
            }
            let info = self.rt.inspect(&row.id).await?;
            if info.state == ContainerState::Running {
                continue;
            }
            let why = match info.state {
                ContainerState::Exited { code } if info.oom_killed => {
                    format!("server ran out of memory (exit code {code})")
                }
                ContainerState::Exited { code } => format!("server crashed (exit code {code})"),
                _ => "server container disappeared".to_owned(),
            };
            let failures = self.bump_failures(&row.id);
            self.event(Some(&row.id), "crash", &why);
            let failed = self.transition(&row, ServerEvent::Failure(why.clone()))?;
            if failures >= self.cfg.max_consecutive_failures {
                tracing::error!(server = %row.id, failures, "giving up restarting");
                self.db.set_auto_start(&row.id, false)?;
                self.rt.remove(&row.id).await?;
                continue;
            }
            tracing::warn!(server = %row.id, failures, reason = %why, "restarting crashed server");
            let row = crate::db::ServerRow {
                state: failed,
                ..row
            };
            self.rt.remove(&row.id).await?;
            self.transition(&row, ServerEvent::Reset)?;
            if let Err(e) = self.start_locked(&row.id).await {
                tracing::error!(server = %row.id, error = %e, "restart after crash failed");
            }
        }
        Ok(())
    }
}
