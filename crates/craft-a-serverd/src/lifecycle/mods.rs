//! Mod management on the lifecycle service: search, plan, install, remove,
//! update check, and recovery of an interrupted change.
//!
//! A change (install or removal) is one transaction, run under the server's
//! lock:
//!
//! 1. resolve and **download into staging** while the server keeps running
//!    (checksum, size and structure verified, nothing visible to the game yet);
//! 2. **stop** the server (if it ran) and take a `pre_change` **snapshot**
//!    while it is quiet, so a rollback loses no play time;
//! 3. write the **journal**, then move files (`mods-txn`, see `mods::txn`);
//! 4. **restart** and wait for the status ping, with a time allowance that
//!    grows with the number of mods;
//! 5. on success record the mods in the database and delete the transaction
//!    directory; on failure **restore the snapshot**, restart the previous
//!    setup and report why.
//!
//! A crash at any point leaves a journal that `reconcile` uses at boot: files
//! are put back (or the records finished if the change had been committed).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use craft_a_server_core::ctl::{
    InstalledModInfo, ModChangeInfo, ModHitInfo, ModInstallPlanInfo, ModLineInfo, ModListInfo,
    ModRemovePlanInfo, ModUpdateInfo,
};
use craft_a_server_core::model::ServerState;

use super::backups::Reason;
use super::{Result, Service, ServiceError, StatusProbe};
use crate::db::{ModChange, ModRow};
use crate::mods::logic::{
    clean_text, dependents_outside, describe_conflict, orphans_after_removal, unique_file_name,
};
use crate::mods::model::{Project, VersionType};
use crate::mods::resolver::{self, InstalledRef, Options, PlannedMod};
use crate::mods::txn::{self, ApplyError, Journal, Stage};
use crate::mods::{ClientSource, ModError, ModrinthClient, SearchQuery, files};
use crate::runtime::ContainerRuntime;

/// Extra start-up time allowed per mod after a change.
const ALLOWANCE_PER_MOD_SECS: u64 = 10;
const MAX_ALLOWANCE_SECS: u64 = 1800;
const SEARCH_PAGE: u32 = 20;
const MAX_TITLE: usize = 80;
const MAX_DESCRIPTION: usize = 300;
const MAX_REASON: usize = 400;

/// A mod resolved for installing, with display names.
struct Prepared {
    plan: resolver::Plan,
    titles: HashMap<String, String>,
    installed_titles: HashMap<String, String>,
}

struct AddSpec {
    planned: PlannedMod,
    title: String,
}

struct RemoveSpec {
    project_id: String,
    title: String,
    file_name: String,
}

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    fn modrinth_client(&self) -> Result<Arc<ModrinthClient>> {
        self.modrinth
            .clone()
            .ok_or(ServiceError::Mods(ModError::NotConfigured))
    }

    fn mods_dir(&self, id: &str) -> PathBuf {
        self.data_dir(id).join("mods")
    }

    fn txn_root(&self, id: &str) -> PathBuf {
        txn::txn_root(&self.server_dir(id))
    }

    // ------------------------------------------------------------ read only

    /// Searches Modrinth for Fabric mods that run on this server.
    pub async fn mods_search(
        &self,
        id: &str,
        text: &str,
        include_unknown: bool,
        offset: u32,
    ) -> Result<Vec<ModHitInfo>> {
        let row = self.get_row(id)?;
        let client = self.modrinth_client()?;
        let text = text.to_owned();
        let mc = row.mc_version;
        let hits = blocking(move || {
            client.search(SearchQuery {
                text: &text,
                mc_version: &mc,
                include_unknown,
                offset,
                limit: SEARCH_PAGE,
            })
        })
        .await??;
        let installed: HashSet<String> = self
            .db
            .list_mods(id)?
            .into_iter()
            .map(|m| m.project_id)
            .collect();
        Ok(hits
            .into_iter()
            .map(|h| ModHitInfo {
                installed: installed.contains(&h.project_id),
                project_id: h.project_id,
                slug: clean_text(&h.slug, MAX_TITLE),
                title: clean_text(&h.title, MAX_TITLE),
                description: clean_text(&h.description, MAX_DESCRIPTION),
                author: clean_text(&h.author, MAX_TITLE),
                downloads: h.downloads,
                server_side: if h.server_side == "required" || h.server_side == "optional" {
                    h.server_side
                } else {
                    "unknown".to_owned()
                },
            })
            .collect())
    }

    /// The recorded mods plus any jars in the folder that we did not install.
    pub async fn mods_list(&self, id: &str) -> Result<ModListInfo> {
        self.get_row(id)?;
        let rows = self.db.list_mods(id)?;
        let dir = self.mods_dir(id);
        let inventory = blocking(move || files::inventory(&dir))
            .await?
            .map_err(ServiceError::io("reading the mods folder"))?;
        let on_disk: HashMap<&str, u64> = inventory
            .iter()
            .map(|e| (e.file_name.as_str(), e.size_bytes))
            .collect();
        let titles: HashMap<&str, &str> = rows
            .iter()
            .map(|r| (r.project_id.as_str(), r.title.as_str()))
            .collect();
        let mut mods = Vec::new();
        for r in &rows {
            let status = match on_disk.get(r.file_name.as_str()) {
                None => "missing",
                Some(size) if *size != r.size_bytes => "size_changed",
                Some(_) => "ok",
            };
            let required_by = self
                .db
                .mod_dependents(id, &r.project_id)?
                .iter()
                .map(|p| {
                    titles
                        .get(p.as_str())
                        .map_or(p.clone(), |t| (*t).to_owned())
                })
                .collect();
            mods.push(InstalledModInfo {
                project_id: r.project_id.clone(),
                title: clean_text(&r.title, MAX_TITLE),
                version_number: clean_text(&r.version_number, 40),
                file_name: r.file_name.clone(),
                size_bytes: r.size_bytes,
                as_dependency: r.as_dependency,
                installed_unix: r.installed_unix,
                status: status.to_owned(),
                required_by,
            });
        }
        let known: HashSet<&str> = rows.iter().map(|r| r.file_name.as_str()).collect();
        let untracked = inventory
            .iter()
            .filter(|e| !known.contains(e.file_name.as_str()))
            .map(|e| clean_text(&e.file_name, 120))
            .collect();
        Ok(ModListInfo { mods, untracked })
    }

    /// Compares installed mods with Modrinth. Only newer *releases* are offered.
    pub async fn mods_check(&self, id: &str) -> Result<Vec<ModUpdateInfo>> {
        let row = self.get_row(id)?;
        let client = self.modrinth_client()?;
        let rows = self.db.list_mods(id)?;
        let hashes: Vec<String> = rows.iter().map(|r| r.sha512.clone()).collect();
        let mc = row.mc_version;
        let latest = blocking(move || client.latest_for_hashes(&hashes, &mc)).await??;
        let mut out = Vec::new();
        for r in rows {
            let Some(v) = latest.get(&r.sha512) else {
                continue;
            };
            if v.project_id == r.project_id
                && v.id != r.version_id
                && v.version_type == VersionType::Release
                && v.ids_are_valid()
            {
                out.push(ModUpdateInfo {
                    project_id: r.project_id,
                    title: clean_text(&r.title, MAX_TITLE),
                    installed_version: clean_text(&r.version_number, 40),
                    latest_version: clean_text(&v.version_number, 40),
                });
            }
        }
        Ok(out)
    }

    /// What installing `project` would do. Changes nothing.
    pub async fn mods_plan(
        &self,
        id: &str,
        project: &str,
        allow_prerelease: bool,
    ) -> Result<ModInstallPlanInfo> {
        let row = self.get_row(id)?;
        let prepared = self.prepare_install(id, project, allow_prerelease).await?;
        let title = |p: &str| prepared.title_of(p);
        let mut lines: Vec<ModLineInfo> = prepared
            .plan
            .to_install
            .iter()
            .map(|m| line(m, &title(&m.project_id)))
            .collect();
        let target = lines.pop().ok_or_else(|| {
            ServiceError::Mods(ModError::BadResponse("the plan has no mod in it".into()))
        })?;
        let replaces = prepared
            .plan
            .to_install
            .last()
            .filter(|m| m.replaces.is_some())
            .and_then(|m| prepared.installed_titles.get(&m.project_id).cloned());
        Ok(ModInstallPlanInfo {
            server_id: row.id,
            mc_version: row.mc_version,
            server_running: row.state == ServerState::Running,
            target,
            dependencies: lines,
            already_installed: prepared.plan.satisfied.iter().map(|p| title(p)).collect(),
            optional: prepared.plan.optional.iter().map(|p| title(p)).collect(),
            conflicts: prepared
                .plan
                .conflicts
                .iter()
                .map(|c| describe_conflict(c, &title))
                .collect(),
            replaces,
            unresolved: u32::try_from(prepared.plan.unresolved).unwrap_or(u32::MAX),
        })
    }

    /// What removing `project` would do. Changes nothing.
    pub fn mods_remove_plan(&self, id: &str, project: &str) -> Result<ModRemovePlanInfo> {
        let server = self.get_row(id)?;
        let rows = self.db.list_mods(id)?;
        let target = find_installed(&rows, project)?;
        let graph = self.mod_graph(id, &rows)?;
        let targets = vec![target.project_id.clone()];
        let dependents = dependents_outside(&targets, &graph.requires);
        let orphans = orphans_after_removal(&targets, &graph.as_dependency, &graph.requires);
        let title = |p: &String| {
            clean_text(
                graph
                    .titles
                    .get(p.as_str())
                    .map_or(p.as_str(), String::as_str),
                MAX_TITLE,
            )
        };
        Ok(ModRemovePlanInfo {
            server_id: server.id,
            project_id: target.project_id.clone(),
            title: clean_text(&target.title, MAX_TITLE),
            server_running: server.state == ServerState::Running,
            dependents: dependents.iter().map(title).collect(),
            orphans: orphans.iter().map(title).collect(),
        })
    }

    // -------------------------------------------------------------- changes

    /// Installs `project` and its required dependencies (an update when an
    /// older build is installed). See the module documentation for the steps.
    pub async fn mods_install(
        &self,
        id: &str,
        project: &str,
        allow_prerelease: bool,
    ) -> Result<ModChangeInfo> {
        let _guard = self.lock(id).await;
        self.recover_mod_change_locked(id).await;
        let prepared = self.prepare_install(id, project, allow_prerelease).await?;
        if !prepared.plan.conflicts.is_empty() {
            let title = |p: &str| prepared.title_of(p);
            let text = prepared
                .plan
                .conflicts
                .iter()
                .map(|c| describe_conflict(c, &title))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ModError::Conflicts(format!("this cannot be installed: {text}")).into());
        }
        let adds: Vec<AddSpec> = prepared
            .plan
            .to_install
            .iter()
            .map(|m| AddSpec {
                planned: m.clone(),
                title: prepared.title_of(&m.project_id),
            })
            .collect();
        let rows = self.db.list_mods(id)?;
        let removes = adds
            .iter()
            .filter_map(|a| {
                let old = rows.iter().find(|r| r.project_id == a.planned.project_id)?;
                Some(RemoveSpec {
                    project_id: old.project_id.clone(),
                    title: old.title.clone(),
                    file_name: old.file_name.clone(),
                })
            })
            .collect();
        self.run_change(id, adds, removes).await
    }

    /// Removes a mod. Refuses while other mods need it unless `force`.
    pub async fn mods_remove(
        &self,
        id: &str,
        project: &str,
        force: bool,
        remove_orphans: bool,
    ) -> Result<ModChangeInfo> {
        let _guard = self.lock(id).await;
        self.recover_mod_change_locked(id).await;
        let rows = self.db.list_mods(id)?;
        let target = find_installed(&rows, project)?;
        let graph = self.mod_graph(id, &rows)?;
        let mut gone = vec![target.project_id.clone()];
        let dependents = dependents_outside(&gone, &graph.requires);
        if !dependents.is_empty() && !force {
            let names = dependents
                .iter()
                .map(|p| {
                    graph
                        .titles
                        .get(p.as_str())
                        .cloned()
                        .unwrap_or_else(|| p.clone())
                })
                .map(|t| clean_text(&t, MAX_TITLE))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ModError::HasDependents {
                title: clean_text(&target.title, MAX_TITLE),
                dependents: names,
            }
            .into());
        }
        if remove_orphans {
            gone.extend(orphans_after_removal(
                &gone,
                &graph.as_dependency,
                &graph.requires,
            ));
        }
        let removes: Vec<RemoveSpec> = rows
            .iter()
            .filter(|r| gone.contains(&r.project_id))
            .map(|r| RemoveSpec {
                project_id: r.project_id.clone(),
                title: r.title.clone(),
                file_name: r.file_name.clone(),
            })
            .collect();
        self.run_change(id, Vec::new(), removes).await
    }

    // ---------------------------------------------------------- the machine

    async fn prepare_install(
        &self,
        id: &str,
        project: &str,
        allow_prerelease: bool,
    ) -> Result<Prepared> {
        let row = self.get_row(id)?;
        let client = self.modrinth_client()?;
        let rows = self.db.list_mods(id)?;
        let installed: Vec<InstalledRef> = rows
            .iter()
            .map(|r| InstalledRef {
                project_id: r.project_id.clone(),
                version_id: r.version_id.clone(),
                file_name: r.file_name.clone(),
            })
            .collect();
        let (mc, project) = (row.mc_version.clone(), project.to_owned());
        let (plan, titles) = blocking(move || -> std::result::Result<_, ModError> {
            let root: Project = client
                .project(&project)?
                .ok_or_else(|| ModError::ProjectNotFound(clean_text(&project, 64)))?;
            let name = clean_text(&root.title, MAX_TITLE);
            if root.project_type != "mod" {
                return Err(ModError::NotAMod(name));
            }
            if root.server_side == "unsupported" {
                return Err(ModError::ClientOnly(name));
            }
            let source = ClientSource {
                client: &client,
                mc_version: &mc,
            };
            let plan = resolver::resolve(
                &source,
                Options {
                    mc_version: &mc,
                    allow_prerelease,
                },
                &installed,
                &root.id,
            )
            .map_err(|e| match e {
                ModError::AlreadyInstalled(_) => ModError::AlreadyInstalled(name.clone()),
                other => other,
            })?;
            let mut ids: BTreeSet<String> = BTreeSet::new();
            ids.extend(plan.to_install.iter().map(|m| m.project_id.clone()));
            ids.extend(plan.satisfied.iter().cloned());
            ids.extend(plan.optional.iter().cloned());
            for c in &plan.conflicts {
                match c {
                    resolver::Conflict::Incompatible { project, with } => {
                        ids.insert(project.clone());
                        ids.insert(with.clone());
                    }
                    resolver::Conflict::VersionPinned { project, .. } => {
                        ids.insert(project.clone());
                    }
                }
            }
            let wanted: Vec<String> = ids.into_iter().collect();
            // Names are for display only: a failed lookup falls back to ids.
            let mut titles: HashMap<String, String> = client
                .projects(&wanted)
                .unwrap_or_default()
                .into_iter()
                .map(|p| (p.id, clean_text(&p.title, MAX_TITLE)))
                .collect();
            titles.insert(root.id, name);
            Ok((plan, titles))
        })
        .await??;
        Ok(Prepared {
            plan,
            titles,
            installed_titles: rows
                .iter()
                .map(|r| {
                    (
                        r.project_id.clone(),
                        format!(
                            "{} {}",
                            clean_text(&r.title, MAX_TITLE),
                            clean_text(&r.version_number, 40)
                        ),
                    )
                })
                .collect(),
        })
    }

    fn mod_graph(&self, id: &str, rows: &[ModRow]) -> Result<ModGraph> {
        let mut graph = ModGraph::default();
        for r in rows {
            graph.titles.insert(r.project_id.clone(), r.title.clone());
            graph
                .as_dependency
                .insert(r.project_id.clone(), r.as_dependency);
            graph.requires.insert(
                r.project_id.clone(),
                self.db.mod_requirements(id, &r.project_id)?,
            );
        }
        Ok(graph)
    }

    pub(super) fn start_allowance(&self, mods_after: usize) -> Duration {
        let extra = (mods_after as u64).saturating_mul(ALLOWANCE_PER_MOD_SECS);
        Duration::from_secs(
            self.cfg
                .start_timeout_secs
                .saturating_add(extra)
                .min(MAX_ALLOWANCE_SECS.max(self.cfg.start_timeout_secs)),
        )
    }

    /// The transaction. The caller holds the server lock.
    #[allow(clippy::too_many_lines)] // one linear story; splitting it would hide the order
    async fn run_change(
        &self,
        id: &str,
        mut adds: Vec<AddSpec>,
        removes: Vec<RemoveSpec>,
    ) -> Result<ModChangeInfo> {
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
        for r in &removes {
            if !files::is_safe_jar_name(&r.file_name) {
                return Err(ModError::invalid("a recorded mod file name is not safe").into());
            }
        }
        self.check_disk_space()?;

        let root = self.txn_root(id);
        let mods_dir = self.mods_dir(id);
        let client = if adds.is_empty() {
            None
        } else {
            Some(self.modrinth_client()?)
        };

        // Fresh transaction directory; leftovers without a journal never moved anything.
        {
            let root = root.clone();
            blocking(move || txn::cleanup(&root).and_then(|()| txn::begin(&root)))
                .await?
                .map_err(ServiceError::io("preparing the mods transaction"))?;
        }

        // Names: unique in the mods folder once the removed jars are gone.
        let inventory = {
            let dir = mods_dir.clone();
            blocking(move || files::inventory(&dir))
                .await?
                .map_err(ServiceError::io("reading the mods folder"))?
        };
        let removing: HashSet<String> = removes
            .iter()
            .map(|r| r.file_name.to_ascii_lowercase())
            .collect();
        let mut taken: HashSet<String> = inventory
            .iter()
            .map(|e| e.file_name.to_ascii_lowercase())
            .filter(|n| !removing.contains(n))
            .collect();
        for add in &mut adds {
            let name =
                unique_file_name(&add.planned.file.file_name, &add.planned.project_id, &taken)
                    .ok_or_else(|| ModError::invalid("could not find a free file name"))?;
            taken.insert(name.to_ascii_lowercase());
            add.planned.file.file_name = name;
        }
        let jars_after = inventory
            .len()
            .saturating_add(adds.len())
            .saturating_sub(removes.len());

        // 1. Download and verify into staging, with the server still running.
        if let Some(client) = &client {
            for add in &adds {
                let (client, file, dir) = (
                    Arc::clone(client),
                    add.planned.file.clone(),
                    txn::staging_dir(&root),
                );
                let result = blocking(move || client.download(&file, &dir)).await;
                let failure = match result {
                    Ok(Ok(_)) => continue,
                    Ok(Err(e)) => ServiceError::Mods(e),
                    Err(e) => e,
                };
                self.discard_txn(&root).await;
                return Err(failure);
            }
        }

        // 2. Quiet the server and take the snapshot.
        if was_running && let Err(e) = self.stop_locked(id).await {
            self.discard_txn(&root).await;
            return Err(e);
        }
        let snapshot = match self.backup_locked(id, Reason::PreChange).await {
            Ok(b) => b,
            Err(e) => {
                self.discard_txn(&root).await;
                if was_running {
                    self.restart_after_abort(id).await;
                }
                return Err(e);
            }
        };

        // 3. Journal, then the file moves.
        let now = self.now();
        let upserts: Vec<ModChange> = adds
            .iter()
            .map(|a| ModChange {
                row: ModRow {
                    server_id: id.to_owned(),
                    project_id: a.planned.project_id.clone(),
                    version_id: a.planned.version_id.clone(),
                    title: a.title.clone(),
                    version_number: clean_text(&a.planned.version_number, 40),
                    file_name: a.planned.file.file_name.clone(),
                    sha512: a.planned.file.sha512.clone(),
                    size_bytes: a.planned.file.size,
                    as_dependency: a.planned.as_dependency,
                    installed_unix: now,
                },
                requires: a.planned.requires.clone(),
            })
            .collect();
        let added: HashSet<&str> = adds.iter().map(|a| a.planned.project_id.as_str()).collect();
        let mut journal = Journal {
            txn: format!("{now}"),
            stage: Stage::Applying,
            backup_id: Some(snapshot.id.clone()),
            adds: adds
                .iter()
                .map(|a| a.planned.file.file_name.clone())
                .collect(),
            removes: removes.iter().map(|r| r.file_name.clone()).collect(),
            remove_projects: removes
                .iter()
                .filter(|r| !added.contains(r.project_id.as_str()))
                .map(|r| r.project_id.clone())
                .collect(),
            upserts,
        };
        let applied = {
            let (root, mods_dir, journal) = (root.clone(), mods_dir.clone(), journal.clone());
            blocking(move || -> std::result::Result<(), ApplyError> {
                txn::write_journal(&root, &journal)?;
                let result = txn::apply(&root, &mods_dir, &journal, &mut |_| true);
                if result.is_err() {
                    let _ = txn::undo(&root, &mods_dir, &journal);
                }
                result
            })
            .await?
        };
        if let Err(e) = applied {
            self.discard_txn(&root).await;
            if was_running {
                self.restart_after_abort(id).await;
            }
            return Err(match e {
                ApplyError::Exists(name) => ModError::Conflicts(format!(
                    "{} is already in the mods folder and was not installed by this program",
                    clean_text(&name, 120)
                ))
                .into(),
                ApplyError::Io(source) => ServiceError::Io {
                    context: "moving mod files into place",
                    source,
                },
                ApplyError::Interrupted => ServiceError::Io {
                    context: "moving mod files into place",
                    source: std::io::Error::other("interrupted"),
                },
            });
        }
        journal.stage = Stage::Applied;
        self.write_journal(&root, &journal).await?;

        // 4. Restart and watch it come up.
        let mut verified = false;
        if was_running {
            match self
                .start_locked_within(id, self.start_allowance(jars_after))
                .await
            {
                Ok(()) => verified = true,
                Err(e) => {
                    let reason = match &e {
                        ServiceError::StartFailed(reason) => clean_text(reason, MAX_REASON),
                        other => {
                            tracing::error!(server = %id, error = %other, "restart after a mod change failed");
                            "the server could not be restarted".to_owned()
                        }
                    };
                    return Err(self
                        .roll_back(
                            id,
                            &root,
                            &mods_dir,
                            &journal,
                            snapshot.id,
                            jars_after,
                            &reason,
                        )
                        .await);
                }
            }
        }

        // 5. Commit.
        journal.stage = Stage::Committed;
        self.write_journal(&root, &journal).await?;
        self.db
            .apply_mod_changes(id, &journal.remove_projects, &journal.upserts)?;
        self.discard_txn(&root).await;
        let summary = format!(
            "+{} -{} ({})",
            adds.len(),
            removes.len(),
            adds.iter()
                .map(|a| clean_text(&a.title, 40))
                .collect::<Vec<_>>()
                .join(", ")
        );
        self.event(Some(id), "mods_changed", &summary);
        let titles: HashMap<&str, &str> = adds
            .iter()
            .map(|a| (a.planned.project_id.as_str(), a.title.as_str()))
            .collect();
        Ok(ModChangeInfo {
            installed: adds
                .iter()
                .map(|a| {
                    line(
                        &a.planned,
                        titles
                            .get(a.planned.project_id.as_str())
                            .copied()
                            .unwrap_or(""),
                    )
                })
                .collect(),
            removed: removes
                .iter()
                .map(|r| clean_text(&r.title, MAX_TITLE))
                .collect(),
            verified,
            backup_id: Some(snapshot.id),
        })
    }

    /// Puts the previous setup back after a failed health check.
    #[allow(clippy::too_many_arguments)]
    async fn roll_back(
        &self,
        id: &str,
        root: &std::path::Path,
        mods_dir: &std::path::Path,
        journal: &Journal,
        snapshot_id: String,
        jars_after: usize,
        reason: &str,
    ) -> ServiceError {
        tracing::warn!(server = %id, %reason, "mod change failed its health check, rolling back");
        self.event(Some(id), "mods_rollback", &clean_text(reason, 200));
        let restored = self.restore_locked(id, &snapshot_id, false).await;
        if let Err(e) = &restored {
            tracing::error!(server = %id, error = %e, "restoring the snapshot failed");
            // Fall back to putting the jars back; the world is left as it is.
            let (root, mods_dir, journal) =
                (root.to_path_buf(), mods_dir.to_path_buf(), journal.clone());
            let _ = blocking(move || txn::undo(&root, &mods_dir, &journal)).await;
        }
        let back_up = self.bring_back_up(id, jars_after).await;
        self.discard_txn(root).await;
        match (restored, back_up) {
            (Ok(()), Ok(())) => ModError::RolledBack {
                reason: reason.to_owned(),
            }
            .into(),
            (restored, back_up) => {
                let mut why = reason.to_owned();
                if restored.is_err() {
                    why.push_str("; the snapshot could not be restored");
                }
                if back_up.is_err() {
                    why.push_str("; the previous setup did not start either");
                }
                ModError::RollbackFailed { reason: why }.into()
            }
        }
    }

    /// Clears a failed start and starts the server again.
    async fn bring_back_up(&self, id: &str, jars: usize) -> Result<()> {
        let row = self.get_row(id)?;
        if matches!(row.state, ServerState::Failed(_)) {
            self.reset_locked(id).await?;
        }
        self.start_locked_within(id, self.start_allowance(jars))
            .await
    }

    /// After an aborted change on a server that was running: start it again.
    async fn restart_after_abort(&self, id: &str) {
        if let Err(e) = self.start_locked(id).await {
            tracing::error!(server = %id, error = %e, "could not restart after an aborted mod change");
        }
    }

    async fn discard_txn(&self, root: &std::path::Path) {
        let root = root.to_path_buf();
        match blocking(move || txn::cleanup(&root)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "could not clean up the mods transaction"),
            Err(e) => tracing::warn!(error = %e, "could not clean up the mods transaction"),
        }
    }

    async fn write_journal(&self, root: &std::path::Path, journal: &Journal) -> Result<()> {
        let (root, journal) = (root.to_path_buf(), journal.clone());
        blocking(move || txn::write_journal(&root, &journal))
            .await?
            .map_err(ServiceError::io("writing the mods journal"))
    }

    // -------------------------------------------------------------- recovery

    /// Finishes or undoes a mod change that a crash interrupted. The caller
    /// holds the server lock. Problems are logged, never fatal.
    pub(crate) async fn recover_mod_change_locked(&self, id: &str) {
        let root = self.txn_root(id);
        let mods_dir = self.mods_dir(id);
        let read = {
            let root = root.clone();
            blocking(move || txn::read_journal(&root)).await
        };
        let journal = match read {
            Ok(Ok(Some(j))) => j,
            Ok(Ok(None)) => {
                // No journal means no file ever moved: leftovers are staging only.
                self.discard_txn(&root).await;
                return;
            }
            Ok(Err(e)) => {
                tracing::error!(server = %id, error = %e, "unreadable mods journal; leaving it for inspection");
                self.event(
                    Some(id),
                    "mods_recovery_failed",
                    "the mods journal is unreadable",
                );
                return;
            }
            Err(e) => {
                tracing::error!(server = %id, error = %e, "cannot read the mods journal");
                return;
            }
        };
        match journal.stage {
            Stage::Applying | Stage::Applied => {
                let (r, m, j) = (root.clone(), mods_dir, journal.clone());
                match blocking(move || txn::undo(&r, &m, &j)).await {
                    Ok(Ok(())) => {
                        self.event(
                            Some(id),
                            "mods_recovered",
                            "an interrupted mod change was undone",
                        );
                        self.discard_txn(&root).await;
                    }
                    Ok(Err(e)) => {
                        tracing::error!(server = %id, error = %e, "could not undo an interrupted mod change");
                        self.event(
                            Some(id),
                            "mods_recovery_failed",
                            "could not put the mods back",
                        );
                    }
                    Err(e) => tracing::error!(server = %id, error = %e, "recovery task failed"),
                }
            }
            Stage::Committed => {
                match self
                    .db
                    .apply_mod_changes(id, &journal.remove_projects, &journal.upserts)
                {
                    Ok(()) => {
                        self.event(
                            Some(id),
                            "mods_recovered",
                            "an interrupted mod change was completed",
                        );
                        self.discard_txn(&root).await;
                    }
                    Err(e) => {
                        tracing::error!(server = %id, error = %e, "could not finish the mod records");
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct ModGraph {
    titles: HashMap<String, String>,
    as_dependency: HashMap<String, bool>,
    requires: HashMap<String, Vec<String>>,
}

impl Prepared {
    fn title_of(&self, project_id: &str) -> String {
        self.titles
            .get(project_id)
            .cloned()
            .unwrap_or_else(|| clean_text(project_id, MAX_TITLE))
    }
}

fn line(m: &PlannedMod, title: &str) -> ModLineInfo {
    ModLineInfo {
        project_id: m.project_id.clone(),
        title: if title.is_empty() {
            m.project_id.clone()
        } else {
            title.to_owned()
        },
        version_number: clean_text(&m.version_number, 40),
        version_type: m.version_type.as_str().to_owned(),
        size_bytes: m.file.size,
        file_name: m.file.file_name.clone(),
        as_dependency: m.as_dependency,
    }
}

/// An installed mod by project id, or by exact title (case-insensitive) when
/// that is unambiguous.
fn find_installed<'a>(rows: &'a [ModRow], wanted: &str) -> Result<&'a ModRow> {
    if let Some(r) = rows.iter().find(|r| r.project_id == wanted) {
        return Ok(r);
    }
    let by_title: Vec<&ModRow> = rows
        .iter()
        .filter(|r| r.title.eq_ignore_ascii_case(wanted))
        .collect();
    match by_title.as_slice() {
        [one] => Ok(one),
        _ => Err(ModError::NotInstalled(clean_text(wanted, 64)).into()),
    }
}

/// Runs blocking work (file and network i/o) off the async threads.
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

// Needs the fake container runtime, which targets Linux (run on WSL on Windows).
#[cfg(all(test, unix))]
mod tests;
