//! Importing a world and its files from an uploaded zip.
//!
//! One import is one transaction under the server's lock, in the same spirit
//! as a mod change:
//!
//! 1. read the archive's table of contents and refuse what cannot work;
//! 2. **stop** the server (if it ran) and take a `pre_change` **snapshot**;
//! 3. build the complete new data directory next to the live one
//!    (`import.tmp`), with the live server's own files carried over;
//! 4. swap it into place with the same two renames a restore uses, so a power
//!    cut leaves either the old or the new data (boot recovery already
//!    handles `data.old`);
//! 5. start the server and wait for the status ping; on failure swap the old
//!    data straight back (no extraction needed) and start the previous setup.
//!
//! The uploaded file is deleted when the import ends, whatever the outcome.

use std::path::PathBuf;

use craft_a_server_core::model::ServerState;

use super::backups::Reason;
use super::{Result, Service, ServiceError, StatusProbe};
use crate::backup::{BackupError, restore};
use crate::mods::logic::clean_text;
use crate::runtime::ContainerRuntime;
use crate::world::{self, ImportPlan, ImportReport, Limits, WorldError};

/// Free space kept on top of the imported files.
const HEADROOM_BYTES: u64 = 512 << 20;
const MAX_REASON: usize = 300;

/// How an import ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldImportOutcome {
    pub report: ImportReport,
    /// The server was started with the new files and answered the status ping.
    pub verified: bool,
    pub backup_id: String,
}

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    /// Where the uploaded zip of a server waits for the import.
    pub fn world_upload_path(&self, id: &str) -> PathBuf {
        self.server_dir(id).join("world-upload.zip")
    }

    /// Where an upload in progress is written.
    pub fn world_upload_partial_path(&self, id: &str) -> PathBuf {
        self.server_dir(id).join("world-upload.part")
    }

    /// Free bytes where the data lives, and the largest upload that fits.
    pub fn world_upload_budget(&self, id: &str) -> Result<u64> {
        self.get_row(id)?;
        let limits = Limits::default();
        let usage = (self.env.disk)(&self.cfg.servers_dir())
            .map_err(ServiceError::io("reading free disk space"))?;
        // The zip, its extracted files and the pre-import backup all need room.
        Ok((usage.free_bytes.saturating_sub(HEADROOM_BYTES) / 3).min(limits.max_zip_bytes))
    }

    /// Deletes the uploaded zip and any upload in progress.
    pub fn discard_world_upload(&self, id: &str) {
        for path in [
            self.world_upload_path(id),
            self.world_upload_partial_path(id),
        ] {
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(error = %e, "could not delete a world upload");
            }
        }
    }

    /// What importing the uploaded zip would bring.
    pub async fn inspect_world_upload(&self, id: &str) -> Result<ImportPlan> {
        self.get_row(id)?;
        let zip = self.world_upload_path(id);
        blocking(move || world::inspect(&zip, &Limits::default()))
            .await?
            .map_err(world_error)
    }

    /// Replaces the world (and mods, configuration, player lists, icon and
    /// gameplay settings the zip brings) with the uploaded zip's.
    pub async fn import_world(&self, id: &str) -> Result<WorldImportOutcome> {
        let _guard = self.lock(id).await;
        let outcome = self.import_world_locked(id).await;
        if !matches!(outcome, Err(ServiceError::WrongState { .. })) {
            self.discard_world_upload(id);
        }
        outcome
    }

    #[allow(clippy::too_many_lines)] // one linear story; splitting it would hide the order
    async fn import_world_locked(&self, id: &str) -> Result<WorldImportOutcome> {
        let row = self.get_row(id)?;
        let was_running = match row.state {
            ServerState::Running => true,
            ServerState::Stopped => false,
            _ => {
                return Err(ServiceError::WrongState {
                    id: id.to_owned(),
                    state: row.state.to_parts().0.to_owned(),
                });
            }
        };

        // 1. Look inside before touching anything.
        let limits = Limits::default();
        let zip = self.world_upload_path(id);
        let plan = {
            let zip = zip.clone();
            blocking(move || world::inspect(&zip, &limits))
                .await?
                .map_err(world_error)?
        };
        self.check_disk_space()?;
        let usage = (self.env.disk)(&self.cfg.servers_dir())
            .map_err(ServiceError::io("reading free disk space"))?;
        if usage.free_bytes < plan.total_bytes.saturating_add(HEADROOM_BYTES) {
            return Err(ServiceError::DiskLow);
        }

        let server_dir = self.server_dir(id);
        let (live, staged, trash, failed) = (
            self.data_dir(id),
            server_dir.join("import.tmp"),
            server_dir.join("data.old"),
            server_dir.join("import.failed"),
        );
        {
            let (staged, trash, failed) = (staged.clone(), trash.clone(), failed.clone());
            blocking(move || -> std::result::Result<(), BackupError> {
                for dir in [&staged, &trash, &failed] {
                    remove_dir_if_exists(dir)?;
                }
                Ok(())
            })
            .await??;
        }

        // 2. Quiet the server and take the snapshot.
        if was_running {
            self.stop_locked(id).await?;
        }
        let snapshot = match self.backup_locked(id, Reason::PreChange).await {
            Ok(b) => b,
            Err(e) => {
                if was_running {
                    self.restart_after_failed_import(id).await;
                }
                return Err(e);
            }
        };

        // 3. Build the new data directory beside the live one.
        let built = {
            let (zip, plan, live, staged) =
                (zip.clone(), plan.clone(), live.clone(), staged.clone());
            blocking(move || world::build_staged(&zip, &plan, &live, &staged, &limits)).await?
        };
        let report = match built {
            Ok(r) => r,
            Err(e) => {
                if was_running {
                    self.restart_after_failed_import(id).await;
                }
                return Err(world_error(e));
            }
        };

        // 4. Swap.
        let swapped = {
            let (live, staged, trash) = (live.clone(), staged.clone(), trash.clone());
            blocking(move || restore::swap_into_place(&live, &staged, &trash)).await?
        };
        if let Err(e) = swapped {
            let staged = staged.clone();
            let _ = blocking(move || remove_dir_if_exists(&staged)).await;
            if was_running {
                self.restart_after_failed_import(id).await;
            }
            return Err(e.into());
        }
        self.event(Some(id), "world_swapped", &snapshot.id);

        // 5. Start and watch it come up.
        let mut verified = false;
        if was_running {
            let jars = report.mods_imported.len();
            match self
                .start_locked_within(id, self.start_allowance(jars))
                .await
            {
                Ok(()) => verified = true,
                Err(e) => {
                    let reason = match &e {
                        ServiceError::StartFailed(reason) => clean_text(reason, MAX_REASON),
                        other => {
                            tracing::error!(server = %id, error = %other, "start after a world import failed");
                            "the server could not be started".to_owned()
                        }
                    };
                    return Err(self
                        .undo_import(id, &live, &trash, &failed, jars, &reason)
                        .await);
                }
            }
        }

        // 6. Commit: the recorded Modrinth mods describe a folder that is gone.
        if report.mods_replaced {
            let projects: Vec<String> = self
                .db
                .list_mods(id)?
                .into_iter()
                .map(|m| m.project_id)
                .collect();
            if !projects.is_empty() {
                self.db.apply_mod_changes(id, &projects, &[])?;
            }
        }
        {
            let trash = trash.clone();
            if let Err(e) = blocking(move || remove_dir_if_exists(&trash)).await? {
                tracing::warn!(error = %e, "could not delete the replaced data directory");
            }
        }
        self.event(
            Some(id),
            "world_imported",
            &format!(
                "{} world files, {} mods, {} config files, {} notes",
                report.world_files,
                report.mods_imported.len(),
                report.config_files,
                report.notes.len()
            ),
        );
        Ok(WorldImportOutcome {
            report,
            verified,
            backup_id: snapshot.id,
        })
    }

    /// Puts the previous data back after the server refused the imported one.
    async fn undo_import(
        &self,
        id: &str,
        live: &std::path::Path,
        trash: &std::path::Path,
        failed: &std::path::Path,
        jars: usize,
        reason: &str,
    ) -> ServiceError {
        tracing::warn!(server = %id, %reason, "world import failed its health check, rolling back");
        self.event(Some(id), "world_rollback", &clean_text(reason, 200));
        let swapped_back = {
            let (live, trash, failed) = (
                live.to_path_buf(),
                trash.to_path_buf(),
                failed.to_path_buf(),
            );
            blocking(move || {
                remove_dir_if_exists(&failed)
                    .and_then(|()| restore::swap_into_place(&live, &trash, &failed))
                    .and_then(|()| remove_dir_if_exists(&failed))
            })
            .await
        };
        let swapped_back = swapped_back.and_then(|r| r.map_err(ServiceError::from));
        if let Err(e) = &swapped_back {
            tracing::error!(server = %id, error = %e, "putting the previous data back failed");
        }
        let back_up = async {
            let row = self.get_row(id)?;
            if matches!(row.state, ServerState::Failed(_)) {
                self.reset_locked(id).await?;
            }
            self.start_locked_within(id, self.start_allowance(jars))
                .await
        }
        .await;
        let mut text = format!(
            "The server did not start with the imported files, so the previous setup was put back. Reason: {reason}"
        );
        if swapped_back.is_err() {
            text = format!(
                "The server did not start with the imported files and the previous setup could not be put back automatically; restore the backup taken before the import. Reason: {reason}"
            );
        } else if back_up.is_err() {
            text.push_str(" The previous setup did not start either.");
        }
        ServiceError::WorldImport(text)
    }

    /// After an import that stopped before changing anything: start again.
    async fn restart_after_failed_import(&self, id: &str) {
        if let Err(e) = self.start_locked(id).await {
            tracing::error!(server = %id, error = %e, "could not restart after an aborted world import");
        }
    }
}

fn world_error(e: WorldError) -> ServiceError {
    match e {
        WorldError::Invalid(text) => ServiceError::WorldImport(text),
        WorldError::Io { context, source } => {
            tracing::error!(%context, error = %source, "world import i/o error");
            ServiceError::Io {
                context: "importing the world",
                source,
            }
        }
    }
}

fn remove_dir_if_exists(path: &std::path::Path) -> std::result::Result<(), BackupError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BackupError::Io {
            context: format!("removing {}", path.display()),
            source,
        }),
    }
}

/// Runs blocking work (file i/o) off the async threads.
async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ServiceError::Io {
            context: "background task",
            source: std::io::Error::other(e.to_string()),
        })
}

#[cfg(all(test, unix))]
mod tests;
