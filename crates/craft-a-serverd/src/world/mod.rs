//! Importing a world (and the files that go with it) from a zip archive.
//!
//! The archive is untrusted input. This module never executes anything from
//! it and never writes outside the directory it is told to build. It works in
//! two steps so the lifecycle service can decide what to do in between:
//!
//! 1. [`inspect`] reads only the zip's central directory and says what an
//!    import would bring: the world, the mods, the configuration, the player
//!    lists, the server icon and a few gameplay settings. Everything else in
//!    the archive is ignored on purpose (server jars, libraries, logs,
//!    scripts, plugins, the EULA file).
//! 2. [`build_staged`] builds the complete new data directory next to the live
//!    one: the imported files plus everything else the live directory has. The
//!    caller swaps it into place atomically, so the live data is never half
//!    changed.
//!
//! Accepted layouts: a whole server folder zipped (with or without one
//! wrapping folder), or just the world folder. The world is found by its
//! `level.dat`, and is always placed in `world/`.

mod props;
mod stage;

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use zip::ZipArchive;

pub use stage::build_staged;

/// Files that hold the player lists, copied as they are once they parse.
pub const LIST_FILES: [&str; 4] = [
    "whitelist.json",
    "ops.json",
    "banned-players.json",
    "banned-ips.json",
];
pub const ICON_FILE: &str = "server-icon.png";
pub const PROPERTIES_FILE: &str = "server.properties";

const MAX_PATH_BYTES: usize = 1024;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_JAR_NAME: usize = 120;

/// Size and count caps. The defaults suit a home server; tests lower them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted archive file.
    pub max_zip_bytes: u64,
    /// Largest total of everything that would be written.
    pub max_total_bytes: u64,
    pub max_entries: usize,
    pub max_mods: usize,
    pub max_mod_bytes: u64,
    pub max_config_bytes: u64,
    pub max_list_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_zip_bytes: 8 << 30,
            max_total_bytes: 24 << 30,
            max_entries: 1_000_000,
            max_mods: 2_000,
            max_mod_bytes: 256 << 20,
            max_config_bytes: 16 << 20,
            max_list_bytes: 8 << 20,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    /// The archive cannot be imported. The text is written for people and
    /// contains no paths of this machine.
    #[error("{0}")]
    Invalid(String),
    #[error("i/o error during {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl WorldError {
    pub(crate) fn invalid(text: impl Into<String>) -> WorldError {
        WorldError::Invalid(text.into())
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> WorldError {
        WorldError::Io {
            context: context.into(),
            source,
        }
    }
}

/// What an import would bring, as found by [`inspect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    /// Folder prefix of the server inside the archive ("" when at the top).
    pub(crate) base: String,
    /// Name of the world folder under `base` ("" when `level.dat` is at `base`).
    pub(crate) world_dir: String,
    pub world_files: u64,
    pub world_bytes: u64,
    /// `(path in the archive, file name it will get)` of every mod jar.
    pub(crate) mod_files: Vec<(String, String)>,
    /// The archive has a `mods/` folder, so the mods are replaced as a whole.
    pub replaces_mods: bool,
    pub config_files: u64,
    pub lists: Vec<&'static str>,
    pub icon: bool,
    pub properties: bool,
    /// Files in the archive that an import does not use.
    pub ignored: u64,
    /// Bytes an import writes (declared sizes).
    pub total_bytes: u64,
}

impl ImportPlan {
    /// File names the mods will have in the server, in archive order.
    pub fn mod_names(&self) -> Vec<&str> {
        self.mod_files.iter().map(|(_, n)| n.as_str()).collect()
    }
}

/// What an import did, for the event log and the person.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub world_files: u64,
    pub mods_imported: Vec<String>,
    pub config_files: u64,
    pub lists: Vec<String>,
    pub icon: bool,
    pub properties_applied: Vec<String>,
    /// Things from the archive that were skipped, and why.
    pub notes: Vec<String>,
    /// The mods folder was replaced (so recorded mods no longer apply).
    pub mods_replaced: bool,
}

/// The kind of a file in the archive, relative to the server folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Class {
    World(String),
    Properties,
    Mod,
    Config(String),
    List(&'static str),
    Icon,
    Ignore,
}

/// Cleans an archive path. `Ok(None)` is a harmless entry to skip,
/// `Err` is a path that must make the whole import fail.
pub(crate) fn clean_name(raw: &str) -> Result<Option<String>, WorldError> {
    // Some Windows tools write backslashes; every component is checked below.
    let name = raw.replace('\\', "/");
    let name = name.trim_end_matches('/');
    if name.is_empty() {
        return Ok(None);
    }
    if name.len() > MAX_PATH_BYTES || name.starts_with('/') {
        return Err(WorldError::invalid("The zip contains an unsafe file path."));
    }
    for part in name.split('/') {
        let bad = part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > MAX_COMPONENT_BYTES
            || part.chars().any(|c| c.is_control() || c == ':');
        if bad {
            return Err(WorldError::invalid("The zip contains an unsafe file path."));
        }
    }
    let first = name.split('/').next().unwrap_or("");
    if first == "__MACOSX" || name.ends_with("/.DS_Store") || name.ends_with("/Thumbs.db") {
        return Ok(None);
    }
    Ok(Some(name.to_owned()))
}

/// Where a file of the archive belongs. `rel` is relative to the server folder.
pub(crate) fn classify(rel: &str, world_dir: &str) -> Class {
    let parts: Vec<&str> = rel.split('/').collect();
    if world_dir.is_empty() {
        return world_class(&parts);
    }
    match parts.as_slice() {
        [first, rest @ ..] if *first == world_dir && !rest.is_empty() => world_class(rest),
        [name] if *name == PROPERTIES_FILE => Class::Properties,
        [name] if *name == ICON_FILE => Class::Icon,
        [name] => LIST_FILES
            .iter()
            .find(|l| *l == name)
            .map_or(Class::Ignore, |l| Class::List(l)),
        ["mods", jar] if jar.to_ascii_lowercase().ends_with(".jar") => Class::Mod,
        ["config", rest @ ..] if !rest.is_empty() && rest.len() <= 8 => {
            Class::Config(rest.join("/"))
        }
        _ => Class::Ignore,
    }
}

fn world_class(parts: &[&str]) -> Class {
    match parts {
        // The game's own lock file; a copied one would only confuse the check.
        [] | ["session.lock"] => Class::Ignore,
        _ => Class::World(parts.join("/")),
    }
}

/// A file name for an imported jar that is safe everywhere: plain characters,
/// a `.jar` ending, not hidden, unique among `taken` (lowercase names).
pub(crate) fn safe_jar_name(raw: &str, taken: &mut HashSet<String>) -> String {
    let mut stem: String = raw
        .get(..raw.len().saturating_sub(".jar".len()))
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    stem.truncate(MAX_JAR_NAME - ".jar".len() - 5);
    if stem.is_empty() || stem.starts_with('.') {
        stem.insert(0, '_');
    }
    let mut candidate = format!("{stem}.jar");
    let mut n = 2_u32;
    while !taken.insert(candidate.to_ascii_lowercase()) {
        candidate = format!("{stem}-{n}.jar");
        n += 1;
    }
    candidate
}

fn open_archive(path: &Path, limits: &Limits) -> Result<ZipArchive<BufReader<File>>, WorldError> {
    let file = File::open(path).map_err(|e| WorldError::io("opening the upload", e))?;
    let len = file
        .metadata()
        .map_err(|e| WorldError::io("reading the upload", e))?
        .len();
    if len > limits.max_zip_bytes {
        return Err(WorldError::invalid("The zip file is too big."));
    }
    ZipArchive::new(BufReader::new(file))
        .map_err(|_| WorldError::invalid("That file is not a valid zip archive."))
}

/// Looks inside the archive and says what an import would bring.
pub fn inspect(path: &Path, limits: &Limits) -> Result<ImportPlan, WorldError> {
    let mut archive = open_archive(path, limits)?;
    if archive.len() > limits.max_entries {
        return Err(WorldError::invalid("The zip has too many files."));
    }

    // (clean name, declared size, compressed size) of every regular file.
    let mut files: Vec<(String, u64, u64)> = Vec::new();
    let mut folders: HashSet<String> = HashSet::new();
    let mut ignored_links = 0_u64;
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|_| WorldError::invalid("The zip is damaged."))?;
        if entry.encrypted() {
            return Err(WorldError::invalid(
                "The zip is password-protected. Create it again without a password.",
            ));
        }
        let raw = entry
            .name()
            .map_err(|_| WorldError::invalid("The zip contains an unreadable file name."))?;
        let Some(name) = clean_name(&raw)? else {
            continue;
        };
        if entry.is_dir() {
            folders.insert(name);
        } else if entry.is_symlink() {
            ignored_links += 1;
        } else {
            files.push((name, entry.size(), entry.compressed_size()));
        }
    }

    let (base, world_dir) = find_world(&files)?;
    let strip = |name: &str| -> Option<String> {
        if base.is_empty() {
            Some(name.to_owned())
        } else {
            name.strip_prefix(base.as_str())
                .and_then(|r| r.strip_prefix('/'))
                .map(str::to_owned)
        }
    };

    let mut plan = ImportPlan {
        base: base.clone(),
        world_dir: world_dir.clone(),
        world_files: 0,
        world_bytes: 0,
        mod_files: Vec::new(),
        replaces_mods: false,
        config_files: 0,
        lists: Vec::new(),
        icon: false,
        properties: false,
        ignored: ignored_links,
        total_bytes: 0,
    };
    let mut taken = HashSet::new();
    for (name, size, compressed) in &files {
        if *size > (1 << 30) && compressed.saturating_mul(2_000) < *size {
            return Err(WorldError::invalid(
                "The zip looks like a decompression bomb and was refused.",
            ));
        }
        let Some(rel) = strip(name) else {
            plan.ignored += 1;
            continue;
        };
        match classify(&rel, &world_dir) {
            Class::World(_) => {
                plan.world_files += 1;
                plan.world_bytes = plan.world_bytes.saturating_add(*size);
            }
            Class::Properties => plan.properties = true,
            Class::Icon => plan.icon = true,
            Class::List(list) => plan.lists.push(list),
            Class::Config(_) => plan.config_files += 1,
            Class::Mod => {
                let file = rel.rsplit('/').next().unwrap_or(&rel);
                let safe = safe_jar_name(file, &mut taken);
                plan.mod_files.push((name.clone(), safe));
            }
            Class::Ignore => {
                plan.ignored += 1;
                continue;
            }
        }
        plan.total_bytes = plan.total_bytes.saturating_add(*size);
    }
    plan.replaces_mods = !plan.mod_files.is_empty()
        || (!world_dir.is_empty()
            && folders
                .iter()
                .any(|f| strip(f).is_some_and(|r| r == "mods")));
    if plan.mod_files.len() > limits.max_mods {
        return Err(WorldError::invalid("The zip has too many mods."));
    }
    if plan.total_bytes > limits.max_total_bytes {
        return Err(WorldError::invalid("The files in the zip are too big."));
    }
    Ok(plan)
}

/// Finds the world: returns `(server folder prefix, world folder name)`.
fn find_world(files: &[(String, u64, u64)]) -> Result<(String, String), WorldError> {
    let mut dirs: Vec<String> = files
        .iter()
        .filter_map(|(name, _, _)| {
            if name == "level.dat" {
                Some(String::new())
            } else {
                name.strip_suffix("/level.dat").map(str::to_owned)
            }
        })
        .filter(|dir| dir.matches('/').count() < 2)
        .collect();
    dirs.sort();
    dirs.dedup();
    let depth = |d: &String| {
        if d.is_empty() {
            0
        } else {
            d.matches('/').count() + 1
        }
    };
    let Some(shallowest) = dirs.iter().map(depth).min() else {
        return Err(WorldError::invalid(
            "No world found in the zip: a Minecraft world has a file called level.dat.",
        ));
    };
    let candidates: Vec<&String> = dirs.iter().filter(|d| depth(d) == shallowest).collect();
    let chosen = match candidates.as_slice() {
        [one] => (*one).clone(),
        many => many
            .iter()
            .find(|d| d.rsplit('/').next() == Some("world"))
            .map(|d| (*d).clone())
            .ok_or_else(|| {
                WorldError::invalid(
                    "The zip has more than one world. Put only the one you want in the zip.",
                )
            })?,
    };
    Ok(match chosen.rsplit_once('/') {
        Some((base, name)) => (base.to_owned(), name.to_owned()),
        None => (String::new(), chosen),
    })
}

#[cfg(test)]
mod tests;
