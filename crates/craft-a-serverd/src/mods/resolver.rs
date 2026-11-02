//! Dependency resolution: pure and deterministic.
//!
//! Input is a root project, what is already installed and a [`VersionSource`]
//! (the network, or a fixture in tests). Output is a [`Plan`]: the versions to
//! install (dependencies first), what was already satisfied, optional extras
//! that are only suggested, and conflicts. Nothing is downloaded or written.
//!
//! Rules (from the Modrinth spike): the version of a project is the newest
//! `release` that lists the `fabric` loader and the server's Minecraft
//! version; `required` dependencies are followed recursively with cycle
//! detection; a dependency that pins a `version_id` gets that version;
//! `incompatible` entries block only when the other mod is installed or
//! planned; `optional` ones are suggested; `embedded` and unknown types are
//! ignored.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::error::ModError;
use super::files::{MAX_MOD_BYTES, sanitize_jar_name};
use super::http::validate_cdn_url;
use super::model::{Dependency, DependencyType, Version, VersionType, valid_id};

/// Most projects one install may pull in.
pub const MAX_PROJECTS: usize = 50;

/// Where versions come from.
pub trait VersionSource {
    /// Versions of a project for Fabric and the server's Minecraft version.
    /// Order does not matter and the list is not trusted: it is filtered again.
    fn project_versions(&self, project_id: &str) -> Result<Vec<Version>, ModError>;
    fn version(&self, version_id: &str) -> Result<Option<Version>, ModError>;
}

/// What is already in the mods directory because of us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRef {
    pub project_id: String,
    pub version_id: String,
    pub file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// Already sanitized.
    pub file_name: String,
    pub url: String,
    pub sha512: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMod {
    pub project_id: String,
    pub version_id: String,
    pub version_number: String,
    pub version_type: VersionType,
    pub file: PlannedFile,
    pub as_dependency: bool,
    /// Project ids of its required dependencies (installed or planned).
    pub requires: Vec<String>,
    /// File of an older version of the same project that this one replaces.
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// `project` declares itself incompatible with `with`, which is installed or planned.
    Incompatible { project: String, with: String },
    /// Two requirements want different versions of the same project.
    VersionPinned {
        project: String,
        wanted_version: String,
        have_version: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub root_project_id: String,
    /// Dependencies first, the requested mod last.
    pub to_install: Vec<PlannedMod>,
    /// Required projects that were already installed.
    pub satisfied: Vec<String>,
    /// Optional dependencies of the planned mods that are not installed.
    pub optional: Vec<String>,
    pub conflicts: Vec<Conflict>,
    /// Dependencies that could not be identified (no project and no version id).
    pub unresolved: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    pub mc_version: &'a str,
    pub allow_prerelease: bool,
}

/// Picks the version of a project to install from a (possibly hostile) list.
pub fn select_version(
    versions: &[Version],
    project: &str,
    opts: Options<'_>,
) -> Result<Version, ModError> {
    let mut usable: Vec<&Version> = versions
        .iter()
        .filter(|v| v.ids_are_valid() && v.supports(opts.mc_version) && v.status == "listed")
        .collect();
    // RFC 3339 timestamps in UTC sort as text; newest first, id as a tiebreak.
    usable.sort_by(|a, b| {
        b.date_published
            .cmp(&a.date_published)
            .then_with(|| a.id.cmp(&b.id))
    });
    let release = usable
        .iter()
        .find(|v| v.version_type == VersionType::Release);
    let chosen = release.or_else(|| {
        if opts.allow_prerelease {
            usable.first()
        } else {
            None
        }
    });
    match chosen {
        Some(v) => Ok((*v).clone()),
        None => Err(ModError::NoCompatibleVersion {
            project: project.to_owned(),
            mc_version: opts.mc_version.to_owned(),
            hint: if usable.is_empty() {
                ""
            } else {
                " (only beta or alpha builds exist; allow pre-releases to use them)"
            },
        }),
    }
}

/// The file of `version` as a [`PlannedFile`], or why it cannot be used.
pub fn planned_file(version: &Version) -> Result<PlannedFile, ModError> {
    let file = version.install_file().ok_or_else(|| {
        ModError::BadResponse(format!("version {} has no file to install", version.id))
    })?;
    let sha512 = file
        .hashes
        .sha512
        .as_deref()
        .map(str::to_ascii_lowercase)
        .filter(|h| h.len() == 128 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            ModError::BadResponse(format!("version {} has no SHA-512 checksum", version.id))
        })?;
    if !validate_cdn_url(&file.url) {
        return Err(ModError::ForbiddenHost(host_of(&file.url)));
    }
    if file.size == 0 || file.size > MAX_MOD_BYTES {
        return Err(ModError::TooLarge {
            file: file.filename.chars().take(80).collect(),
            limit_bytes: MAX_MOD_BYTES,
        });
    }
    Ok(PlannedFile {
        file_name: sanitize_jar_name(&file.filename, &version.project_id, &version.id),
        url: file.url.clone(),
        sha512,
        size: file.size,
    })
}

/// The host part of a URL for an error message (never the whole URL).
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(60)
        .collect()
}

/// Resolves what installing `root_project_id` needs.
pub fn resolve<S: VersionSource>(
    source: &S,
    opts: Options<'_>,
    installed: &[InstalledRef],
    root_project_id: &str,
) -> Result<Plan, ModError> {
    let mut r = Resolver {
        source,
        opts,
        installed: installed
            .iter()
            .map(|i| (i.project_id.as_str(), i))
            .collect(),
        planned: Vec::new(),
        index: HashMap::new(),
        visiting: HashSet::new(),
        satisfied: BTreeSet::new(),
        optional: BTreeSet::new(),
        conflicts: Vec::new(),
        incompatible: Vec::new(),
        unresolved: 0,
    };
    r.visit(root_project_id, None, true)?;
    r.finish(root_project_id)
}

struct Resolver<'a, S> {
    source: &'a S,
    opts: Options<'a>,
    installed: HashMap<&'a str, &'a InstalledRef>,
    planned: Vec<PlannedMod>,
    index: HashMap<String, usize>,
    visiting: HashSet<String>,
    satisfied: BTreeSet<String>,
    optional: BTreeSet<String>,
    conflicts: Vec<Conflict>,
    incompatible: Vec<(String, String)>,
    unresolved: usize,
}

impl<S: VersionSource> Resolver<'_, S> {
    fn visit(
        &mut self,
        project_id: &str,
        pinned: Option<&str>,
        is_root: bool,
    ) -> Result<(), ModError> {
        if !valid_id(project_id) {
            return Err(ModError::BadResponse("a dependency has a bad id".into()));
        }
        if let Some(&at) = self.index.get(project_id) {
            let have = self.planned.get(at).map(|p| p.version_id.clone());
            self.check_pin(project_id, pinned, have.as_deref());
            return Ok(());
        }
        if self.visiting.contains(project_id) {
            // A cycle: the project is already being resolved further up.
            return Ok(());
        }
        let installed = self.installed.get(project_id).copied();
        if let (Some(have), false) = (installed, is_root) {
            self.check_pin(project_id, pinned, Some(have.version_id.as_str()));
            self.satisfied.insert(project_id.to_owned());
            return Ok(());
        }
        if self.planned.len() + self.visiting.len() >= MAX_PROJECTS {
            return Err(ModError::TooMany(MAX_PROJECTS));
        }

        let version = self.pick(project_id, pinned)?;
        if let Some(have) = installed
            && have.version_id == version.id
        {
            return Err(ModError::AlreadyInstalled(project_id.to_owned()));
        }
        let file = planned_file(&version)?;

        self.visiting.insert(project_id.to_owned());
        let mut requires = Vec::new();
        for dep in &version.dependencies {
            match dep.dependency_type {
                DependencyType::Required => {
                    if let Some((target, pin)) = self.target(dep)? {
                        requires.push(target.clone());
                        self.visit(&target, pin.as_deref(), false)?;
                    }
                }
                DependencyType::Optional => {
                    if let Some((target, _)) = self.target(dep)? {
                        self.optional.insert(target);
                    }
                }
                DependencyType::Incompatible => {
                    if let Some((target, _)) = self.target(dep)? {
                        self.incompatible.push((project_id.to_owned(), target));
                    }
                }
                DependencyType::Embedded | DependencyType::Unknown => {}
            }
        }
        self.visiting.remove(project_id);

        requires.sort();
        requires.dedup();
        self.index.insert(project_id.to_owned(), self.planned.len());
        self.planned.push(PlannedMod {
            project_id: project_id.to_owned(),
            version_id: version.id.clone(),
            version_number: version.version_number.clone(),
            version_type: version.version_type,
            file,
            as_dependency: !is_root,
            requires,
            replaces: installed.map(|i| i.file_name.clone()),
        });
        Ok(())
    }

    fn check_pin(&mut self, project_id: &str, pinned: Option<&str>, have: Option<&str>) {
        if let (Some(want), Some(have)) = (pinned, have)
            && want != have
        {
            self.conflicts.push(Conflict::VersionPinned {
                project: project_id.to_owned(),
                wanted_version: want.to_owned(),
                have_version: have.to_owned(),
            });
        }
    }

    /// The version to use: the pinned one, or the best from the project's list.
    fn pick(&self, project_id: &str, pinned: Option<&str>) -> Result<Version, ModError> {
        if let Some(version_id) = pinned {
            let version = self.source.version(version_id)?.ok_or_else(|| {
                ModError::BadResponse(format!("pinned version {version_id} does not exist"))
            })?;
            if !version.ids_are_valid()
                || version.project_id != project_id
                || !version.supports(self.opts.mc_version)
            {
                return Err(ModError::NoCompatibleVersion {
                    project: project_id.to_owned(),
                    mc_version: self.opts.mc_version.to_owned(),
                    hint: " (a mod requires a specific build that does not fit)",
                });
            }
            return Ok(version);
        }
        let versions = self.source.project_versions(project_id)?;
        select_version(&versions, project_id, self.opts)
    }

    /// The project (and pinned version) a dependency entry points at.
    #[allow(clippy::type_complexity)]
    fn target(&mut self, dep: &Dependency) -> Result<Option<(String, Option<String>)>, ModError> {
        match (&dep.project_id, &dep.version_id) {
            (Some(p), v) => {
                if !valid_id(p) || v.as_deref().is_some_and(|v| !valid_id(v)) {
                    return Err(ModError::BadResponse("a dependency has a bad id".into()));
                }
                Ok(Some((p.clone(), v.clone())))
            }
            (None, Some(v)) => {
                if !valid_id(v) {
                    return Err(ModError::BadResponse("a dependency has a bad id".into()));
                }
                let version = self.source.version(v)?.ok_or_else(|| {
                    ModError::BadResponse(format!("dependency version {v} does not exist"))
                })?;
                if !valid_id(&version.project_id) {
                    return Err(ModError::BadResponse("a dependency has a bad id".into()));
                }
                Ok(Some((version.project_id, Some(v.clone()))))
            }
            (None, None) => {
                self.unresolved += 1;
                Ok(None)
            }
        }
    }

    fn finish(mut self, root: &str) -> Result<Plan, ModError> {
        for (project, with) in std::mem::take(&mut self.incompatible) {
            // A mod being replaced by an update no longer counts as installed.
            let replaced = self.index.contains_key(&with);
            let present = replaced || self.installed.contains_key(with.as_str());
            if present {
                self.conflicts
                    .push(Conflict::Incompatible { project, with });
            }
        }
        self.optional
            .retain(|p| !self.index.contains_key(p) && !self.installed.contains_key(p.as_str()));
        Ok(Plan {
            root_project_id: root.to_owned(),
            to_install: self.planned,
            satisfied: self.satisfied.into_iter().collect(),
            optional: self.optional.into_iter().collect(),
            conflicts: self.conflicts,
            unresolved: self.unresolved,
        })
    }
}

#[cfg(test)]
mod tests;
