//! Backups, restores, retention and the daily schedule.

use std::path::{Path, PathBuf};

use craft_a_server_core::model::ServerState;

use super::server::ignore_not_found;
use super::{Result, Service, ServiceError, StatusProbe};
use crate::backup::{
    self, BackupKind, BackupMeta, ExtractLimits, RetentionPolicy, archive, restore, retention,
};
use crate::db::{BackupRow, ServerRow};
use crate::runtime::ContainerRuntime;

/// Top-level entries that are regenerated or only diagnostic.
const EXCLUDE: &[&str] = &["logs", "crash-reports", "cache", ".fabric"];
const SECONDS_PER_DAY: i64 = 86_400;

/// Why a backup is being taken; stored as text and mapped to a retention class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Daily,
    Manual,
    PreChange,
    PreRestore,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Daily => "daily",
            Reason::Manual => "manual",
            Reason::PreChange => "pre_change",
            Reason::PreRestore => "pre_restore",
        }
    }

    fn kind(self) -> BackupKind {
        match self {
            Reason::Daily => BackupKind::Daily,
            Reason::Manual => BackupKind::Manual,
            Reason::PreChange => BackupKind::PreChange,
            Reason::PreRestore => BackupKind::PreRestore,
        }
    }

    fn from_str(s: &str) -> Reason {
        match s {
            "daily" => Reason::Daily,
            "pre_change" => Reason::PreChange,
            "pre_restore" => Reason::PreRestore,
            // Unknown text is treated as manual so retention never deletes it.
            _ => Reason::Manual,
        }
    }
}

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    /// Takes, verifies and records a backup, then applies retention.
    pub async fn backup(&self, id: &str, reason: Reason) -> Result<BackupRow> {
        let _guard = self.lock(id).await;
        self.backup_locked(id, reason).await
    }

    pub(crate) async fn backup_locked(&self, id: &str, reason: Reason) -> Result<BackupRow> {
        let row = self.get_row(id)?;
        self.check_disk_space()?;

        let data = self.data_dir(id);
        let dest = self.server_backups_dir(id);
        std::fs::create_dir_all(&dest)
            .map_err(ServiceError::io("creating the backup directory"))?;

        let created = self.now();
        let name = format!("{id}-{created}-{}", reason.as_str());
        let live = row.state == ServerState::Running;

        // Saving off pauses autosave so the files on disk are consistent;
        // `save-on` must run however the copy ends.
        if live {
            self.rt.rcon(id, "save-off").await?;
            if let Err(e) = self.rt.rcon(id, "save-all flush").await {
                self.save_on(id).await;
                return Err(e.into());
            }
        }
        let archived = {
            let (data, dest, name) = (data.clone(), dest.clone(), name.clone());
            tokio::task::spawn_blocking(move || archive::create(&data, &dest, &name, EXCLUDE)).await
        };
        if live {
            self.save_on(id).await;
        }
        let info = archived.map_err(|e| join_error(&e))??;

        let required = required_files(&data);
        let verified = {
            let (path, sha) = (info.path.clone(), info.sha256_hex.clone());
            tokio::task::spawn_blocking(move || {
                let required: Vec<&str> = required.iter().map(String::as_str).collect();
                archive::verify(&path, Some(&sha), &required)
            })
            .await
        };
        let verified = verified.map_err(|e| join_error(&e))?;
        if let Err(e) = &verified {
            // A backup that does not verify is worse than none: drop it.
            let _ = std::fs::remove_file(&info.path);
            self.event(Some(id), "backup_failed", &e.to_string());
        }
        verified?;

        let record = BackupRow {
            id: name.clone(),
            server_id: id.to_owned(),
            kind: reason.as_str().to_owned(),
            file_name: format!("{name}.tar.zst"),
            size_bytes: info.size_bytes,
            sha256: info.sha256_hex,
            created_unix: created,
            verified: true,
        };
        self.db.insert_backup(&record)?;
        self.event(
            Some(id),
            "backup",
            &format!("{} ({} bytes)", record.id, record.size_bytes),
        );
        self.apply_retention(&row);
        Ok(record)
    }

    async fn save_on(&self, id: &str) {
        if let Err(e) = self.rt.rcon(id, "save-on").await {
            tracing::error!(server = %id, error = %e, "could not re-enable saving");
            self.event(Some(id), "save_on_failed", &e.to_string());
        }
    }

    fn check_disk_space(&self) -> Result<()> {
        let dir = self.cfg.backups_dir();
        std::fs::create_dir_all(&dir)
            .map_err(ServiceError::io("creating the backups directory"))?;
        let usage = (self.env.disk)(&dir).map_err(ServiceError::io("reading free disk space"))?;
        let fraction = f64::from(self.cfg.min_free_disk_percent) / 100.0;
        if backup::space_ok(usage.free_bytes, usage.total_bytes, fraction) {
            Ok(())
        } else {
            Err(ServiceError::DiskLow)
        }
    }

    /// Deletes backups the retention policy no longer needs. Failures are
    /// logged, never fatal: keeping an extra backup is always safe.
    fn apply_retention(&self, server: &ServerRow) {
        let rows = match self.db.list_backups(&server.id) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "retention: cannot list backups");
                return;
            }
        };
        let metas: Vec<BackupMeta> = rows
            .iter()
            .map(|b| BackupMeta {
                id: b.id.clone(),
                created_unix: b.created_unix,
                kind: Reason::from_str(&b.kind).kind(),
                verified: b.verified,
            })
            .collect();
        let doomed = retention::select_deletions(&metas, self.now(), &RetentionPolicy::default());
        for id in doomed {
            let Some(row) = rows.iter().find(|b| b.id == id) else {
                continue;
            };
            let path = self.backup_path(&server.id, &row.file_name);
            if let Err(e) =
                std::fs::remove_file(&path).or_else(ignore_not_found("removing a backup file"))
            {
                tracing::warn!(error = %e, backup = %id, "retention: cannot delete file");
                continue;
            }
            if let Err(e) = self.db.delete_backup(&id) {
                tracing::warn!(error = %e, backup = %id, "retention: cannot delete record");
            } else {
                self.event(Some(&server.id), "backup_pruned", &id);
            }
        }
    }

    pub(crate) fn backup_path(&self, server_id: &str, file_name: &str) -> PathBuf {
        self.server_backups_dir(server_id).join(file_name)
    }

    pub fn list_backups(&self, id: &str) -> Result<Vec<BackupRow>> {
        self.get_row(id)?;
        Ok(self.db.list_backups(id)?)
    }

    /// Restores a backup: snapshot the current state first, swap the data in,
    /// and start again if the server was running.
    pub async fn restore(&self, id: &str, backup_id: &str) -> Result<()> {
        let _guard = self.lock(id).await;
        let row = self.get_row(id)?;
        let backup = self
            .db
            .get_backup(backup_id)?
            .filter(|b| b.server_id == id)
            .ok_or_else(|| ServiceError::NotFound(backup_id.to_owned()))?;
        let archive_path = self.backup_path(id, &backup.file_name);

        // Prove the archive is good before touching anything.
        {
            let (path, sha) = (archive_path.clone(), backup.sha256.clone());
            tokio::task::spawn_blocking(move || archive::verify(&path, Some(&sha), &[]))
                .await
                .map_err(|e| join_error(&e))??;
        }

        let was_running = row.state == ServerState::Running;
        if matches!(
            row.state,
            ServerState::Starting | ServerState::Stopping | ServerState::Creating
        ) {
            return Err(ServiceError::WrongState {
                id: id.to_owned(),
                state: row.state.to_parts().0.to_owned(),
            });
        }

        // Safety net: the state we are about to replace.
        self.backup_locked(id, Reason::PreRestore).await?;
        if was_running {
            self.stop_locked(id).await?;
        }

        let server_dir = self.server_dir(id);
        let (live, staged, trash) = (
            self.data_dir(id),
            server_dir.join("restore.tmp"),
            server_dir.join("data.old"),
        );
        let limits = ExtractLimits::default();
        {
            let (staged, trash, archive_path) = (staged.clone(), trash.clone(), archive_path);
            tokio::task::spawn_blocking(move || -> std::result::Result<(), backup::BackupError> {
                remove_dir_if_exists(&staged)?;
                remove_dir_if_exists(&trash)?;
                restore::extract_safely(&archive_path, &staged, &limits)
            })
            .await
            .map_err(|e| join_error(&e))??;
        }
        {
            let (live, staged, trash) = (live.clone(), staged.clone(), trash.clone());
            tokio::task::spawn_blocking(move || restore::swap_into_place(&live, &staged, &trash))
                .await
                .map_err(|e| join_error(&e))??;
        }
        if let Err(e) = remove_dir_if_exists(&trash) {
            tracing::warn!(error = %e, "could not delete the replaced data directory");
        }
        self.event(Some(id), "restored", backup_id);

        if was_running {
            self.start_locked(id).await?;
        }
        Ok(())
    }

    /// Backups that should run now because the daily slot has passed without one.
    ///
    /// Covers a machine that was off at the scheduled hour: the missed backup
    /// runs once, as soon as the daemon is up.
    pub async fn run_due_backups(&self) -> Vec<(String, Result<BackupRow>)> {
        let slot = last_slot(self.now(), self.cfg.daily_backup_hour_utc);
        let mut results = Vec::new();
        let servers = match self.db.list_servers() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "scheduler cannot list servers");
                return results;
            }
        };
        for server in servers {
            if !matches!(server.state, ServerState::Running | ServerState::Stopped) {
                continue;
            }
            // A fresh, never-started server has nothing worth saving yet.
            if !self.data_dir(&server.id).join("world").exists() {
                continue;
            }
            let recent = self
                .db
                .list_backups(&server.id)
                .map(|bs| bs.iter().any(|b| b.created_unix >= slot))
                .unwrap_or(true);
            if recent {
                continue;
            }
            let outcome = self.backup(&server.id, Reason::Daily).await;
            if let Err(e) = &outcome {
                tracing::error!(server = %server.id, error = %e, "scheduled backup failed");
            }
            results.push((server.id, outcome));
        }
        results
    }
}

/// The most recent instant at `hour`:00 UTC that is not after `now`.
pub fn last_slot(now_unix: i64, hour_utc: u8) -> i64 {
    let day_start = now_unix.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY;
    let today = day_start + i64::from(hour_utc) * 3600;
    if today <= now_unix {
        today
    } else {
        today - SECONDS_PER_DAY
    }
}

/// A world that exists must have its `level.dat`; a never-started server has none.
fn required_files(data: &Path) -> Vec<String> {
    if data.join("world").is_dir() {
        vec!["world/level.dat".to_owned()]
    } else {
        Vec::new()
    }
}

fn remove_dir_if_exists(path: &Path) -> std::result::Result<(), backup::BackupError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(backup::BackupError::Io {
            context: format!("removing {}", path.display()),
            source,
        }),
    }
}

fn join_error(e: &tokio::task::JoinError) -> ServiceError {
    ServiceError::Io {
        context: "background task",
        source: std::io::Error::other(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_slot_before_and_after_the_hour() {
        let day = 20_000 * SECONDS_PER_DAY;
        assert_eq!(last_slot(day + 5 * 3600, 4), day + 4 * 3600);
        assert_eq!(
            last_slot(day + 3 * 3600, 4),
            day - SECONDS_PER_DAY + 4 * 3600
        );
        assert_eq!(last_slot(day + 4 * 3600, 4), day + 4 * 3600);
        assert_eq!(last_slot(day, 0), day);
    }

    #[test]
    fn reason_roundtrip_and_unknown_is_manual() {
        for r in [
            Reason::Daily,
            Reason::Manual,
            Reason::PreChange,
            Reason::PreRestore,
        ] {
            assert_eq!(Reason::from_str(r.as_str()), r);
        }
        assert_eq!(Reason::from_str("???"), Reason::Manual);
    }
}
