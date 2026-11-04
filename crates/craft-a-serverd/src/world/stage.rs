//! Building the new data directory for an import.
//!
//! The result is a complete data directory next to the live one: everything
//! the live directory has that the import does not replace (the server jar,
//! libraries, `eula.txt`, ...) is hard-linked or copied in, and the imported
//! files are extracted on top. Nothing here touches the live directory.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

use zip::ZipArchive;

use super::{
    Class, ICON_FILE, ImportPlan, ImportReport, LIST_FILES, Limits, PROPERTIES_FILE, WorldError,
    classify, clean_name, props,
};
use crate::mods::files::check_fabric_jar;

const MAX_PROPERTIES_BYTES: u64 = 1 << 20;
const MAX_ICON_BYTES: u64 = 1 << 20;
const MAX_NOTES: usize = 20;
const MAX_TREE_DEPTH: usize = 64;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Builds `staged` (which must not exist) from `live` and the archive.
///
/// On any failure `staged` is removed again.
pub fn build_staged(
    zip: &Path,
    plan: &ImportPlan,
    live: &Path,
    staged: &Path,
    limits: &Limits,
) -> Result<ImportReport, WorldError> {
    match fs::create_dir(staged) {
        Ok(()) => {}
        Err(e) => return Err(WorldError::io("creating the import directory", e)),
    }
    set_dir_mode(staged);
    let result = build(zip, plan, live, staged, limits);
    if result.is_err() {
        let _ = fs::remove_dir_all(staged);
    }
    result
}

fn build(
    zip: &Path,
    plan: &ImportPlan,
    live: &Path,
    staged: &Path,
    limits: &Limits,
) -> Result<ImportReport, WorldError> {
    let mut report = ImportReport {
        mods_replaced: plan.replaces_mods,
        ..ImportReport::default()
    };
    carry_over(live, staged, plan, &mut report)?;
    if plan.replaces_mods {
        make_dir(&staged.join("mods"))?;
    }

    let file = File::open(zip).map_err(|e| WorldError::io("opening the upload", e))?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|_| WorldError::invalid("That file is not a valid zip archive."))?;
    let mod_names: HashMap<&str, &str> = plan
        .mod_files
        .iter()
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect();

    let mut imported_properties: Option<String> = None;
    let mut written_total = 0_u64;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|_| WorldError::invalid("The zip is damaged or password-protected."))?;
        if entry.is_dir() || entry.is_symlink() {
            continue;
        }
        let raw = entry
            .name()
            .map_err(|_| WorldError::invalid("The zip contains an unreadable file name."))?
            .into_owned();
        let Some(name) = clean_name(&raw)? else {
            continue;
        };
        let rel = if plan.base.is_empty() {
            name.clone()
        } else {
            match name
                .strip_prefix(plan.base.as_str())
                .and_then(|r| r.strip_prefix('/'))
            {
                Some(r) => r.to_owned(),
                None => continue,
            }
        };
        let declared = entry.size();
        match classify(&rel, &plan.world_dir) {
            Class::Ignore => {}
            Class::World(inner) => {
                written_total = written_total.saturating_add(declared);
                check_total(written_total, limits)?;
                write_entry(&mut entry, &staged.join("world"), &inner, declared)?;
                report.world_files += 1;
            }
            Class::Config(inner) => {
                if declared > limits.max_config_bytes {
                    note(
                        &mut report,
                        "A configuration file was too big and was skipped.",
                    );
                    continue;
                }
                written_total = written_total.saturating_add(declared);
                check_total(written_total, limits)?;
                write_entry(&mut entry, &staged.join("config"), &inner, declared)?;
                report.config_files += 1;
            }
            Class::Mod => {
                let Some(target) = mod_names.get(name.as_str()) else {
                    continue;
                };
                if declared > limits.max_mod_bytes {
                    note(
                        &mut report,
                        &format!("{target} is too big and was skipped."),
                    );
                    continue;
                }
                written_total = written_total.saturating_add(declared);
                check_total(written_total, limits)?;
                let dir = staged.join("mods");
                write_entry(&mut entry, &dir, target, declared)?;
                let path = dir.join(target);
                match check_fabric_jar(&path) {
                    Ok(()) => report.mods_imported.push((*target).to_owned()),
                    Err(why) => {
                        let _ = fs::remove_file(&path);
                        note(
                            &mut report,
                            &format!("{target} was skipped: it is not a Fabric mod ({why})."),
                        );
                    }
                }
            }
            Class::List(list) => {
                let Some(bytes) = read_small(&mut entry, declared, limits.max_list_bytes) else {
                    note(&mut report, &format!("{list} was too big and was skipped."));
                    keep_live(live, staged, list, &mut report)?;
                    continue;
                };
                let is_array =
                    serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|v| v.is_array());
                if !is_array {
                    note(
                        &mut report,
                        &format!("{list} is not a valid list and was skipped."),
                    );
                    keep_live(live, staged, list, &mut report)?;
                    continue;
                }
                write_bytes(&staged.join(list), &bytes)?;
                report.lists.push(list.to_owned());
            }
            Class::Icon => {
                let Some(bytes) = read_small(&mut entry, declared, MAX_ICON_BYTES) else {
                    note(&mut report, "The server icon was too big and was skipped.");
                    keep_live(live, staged, ICON_FILE, &mut report)?;
                    continue;
                };
                if !is_server_icon(&bytes) {
                    note(
                        &mut report,
                        "The server icon was skipped: it must be a 64x64 PNG image.",
                    );
                    keep_live(live, staged, ICON_FILE, &mut report)?;
                    continue;
                }
                write_bytes(&staged.join(ICON_FILE), &bytes)?;
                report.icon = true;
            }
            Class::Properties => {
                if let Some(bytes) = read_small(&mut entry, declared, MAX_PROPERTIES_BYTES) {
                    imported_properties = Some(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
        }
    }

    // A world without a readable level.dat would make the game create a new one.
    let level = staged.join("world").join("level.dat");
    let mut magic = [0_u8; 2];
    let readable = File::open(&level)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok();
    if !readable || magic != GZIP_MAGIC {
        return Err(WorldError::invalid(
            "The world's level.dat is not a valid Minecraft file.",
        ));
    }

    write_properties(live, staged, imported_properties.as_deref(), &mut report)?;
    sync_tree_root(staged);
    Ok(report)
}

/// Keeps the live server's own copy of a file whose imported version was refused.
fn keep_live(
    live: &Path,
    staged: &Path,
    name: &str,
    report: &mut ImportReport,
) -> Result<(), WorldError> {
    let src = live.join(name);
    if src.is_file() {
        link_tree(&src, &staged.join(name), 0, report)?;
    }
    Ok(())
}

fn check_total(written: u64, limits: &Limits) -> Result<(), WorldError> {
    if written > limits.max_total_bytes {
        Err(WorldError::invalid("The files in the zip are too big."))
    } else {
        Ok(())
    }
}

fn note(report: &mut ImportReport, text: &str) {
    if report.notes.len() < MAX_NOTES {
        report.notes.push(text.to_owned());
    }
}

/// True for a 64x64 PNG, which is what the game accepts as a server icon.
fn is_server_icon(bytes: &[u8]) -> bool {
    let dim = |at: usize| -> Option<u32> {
        let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
        Some(u32::from_be_bytes(raw))
    };
    bytes.get(..8) == Some(&PNG_MAGIC[..])
        && bytes.get(12..16) == Some(b"IHDR".as_slice())
        && dim(16) == Some(64)
        && dim(20) == Some(64)
}

fn write_properties(
    live: &Path,
    staged: &Path,
    imported: Option<&str>,
    report: &mut ImportReport,
) -> Result<(), WorldError> {
    let live_text = match fs::read_to_string(live.join(PROPERTIES_FILE)) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(WorldError::io("reading server.properties", e)),
    };
    match (live_text, imported) {
        (Some(live_text), Some(imported)) => {
            let (merged, applied) = props::merge(&live_text, imported);
            write_bytes(&staged.join(PROPERTIES_FILE), merged.as_bytes())?;
            report.properties_applied = applied;
        }
        (Some(live_text), None) => {
            write_bytes(&staged.join(PROPERTIES_FILE), live_text.as_bytes())?;
        }
        // The server has never started, so there is no file to adjust yet.
        (None, _) => {}
    }
    Ok(())
}

/// Reads a small entry fully; `None` when it is over `max`.
fn read_small(entry: &mut impl Read, declared: u64, max: u64) -> Option<Vec<u8>> {
    if declared > max {
        return None;
    }
    let mut out = Vec::new();
    entry.take(max + 1).read_to_end(&mut out).ok()?;
    (out.len() as u64 <= max).then_some(out)
}

/// Hard-links or copies everything the import does not replace.
fn carry_over(
    live: &Path,
    staged: &Path,
    plan: &ImportPlan,
    report: &mut ImportReport,
) -> Result<(), WorldError> {
    let entries = match fs::read_dir(live) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(WorldError::io("reading the server files", e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| WorldError::io("reading the server files", e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let replaced = match name.as_str() {
            "world" | "logs" | "crash-reports" | PROPERTIES_FILE => true,
            "mods" => plan.replaces_mods,
            "config" => plan.config_files > 0,
            ICON_FILE => plan.icon,
            other => LIST_FILES.contains(&other) && plan.lists.contains(&other),
        };
        if replaced {
            continue;
        }
        link_tree(&entry.path(), &staged.join(&name), 0, report)?;
    }
    Ok(())
}

fn link_tree(
    src: &Path,
    dst: &Path,
    depth: usize,
    report: &mut ImportReport,
) -> Result<(), WorldError> {
    if depth > MAX_TREE_DEPTH {
        return Err(WorldError::invalid(
            "The server folder is nested too deeply.",
        ));
    }
    let kind = fs::symlink_metadata(src)
        .map_err(|e| WorldError::io("reading the server files", e))?
        .file_type();
    if kind.is_dir() {
        make_dir(dst)?;
        for child in fs::read_dir(src).map_err(|e| WorldError::io("reading the server files", e))? {
            let child = child.map_err(|e| WorldError::io("reading the server files", e))?;
            link_tree(
                &child.path(),
                &dst.join(child.file_name()),
                depth + 1,
                report,
            )?;
        }
    } else if kind.is_file() {
        if fs::hard_link(src, dst).is_err() {
            fs::copy(src, dst).map_err(|e| WorldError::io("copying a server file", e))?;
        }
    } else {
        tracing::warn!(path = %src.display(), "world import: skipping a link or special file");
        note(report, "A link in the server folder was not carried over.");
    }
    Ok(())
}

fn make_dir(path: &Path) -> Result<(), WorldError> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path)
        .map_err(|e| WorldError::io("creating a folder", e))
}

fn set_dir_mode(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn create_file(path: &Path) -> Result<File, WorldError> {
    if let Some(parent) = path.parent() {
        make_dir(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => {
            WorldError::invalid("The zip contains the same file twice, or clashing names.")
        }
        _ => WorldError::io("creating a file", e),
    })
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), WorldError> {
    let mut file = create_file(path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| WorldError::io("writing a file", e))
}

/// Extracts one entry to `dir/rel`, refusing more bytes than it declared.
fn write_entry(
    entry: &mut impl Read,
    dir: &Path,
    rel: &str,
    declared: u64,
) -> Result<(), WorldError> {
    let mut dest = dir.to_path_buf();
    dest.extend(rel.split('/'));
    if !dest.starts_with(dir) {
        return Err(WorldError::invalid("The zip contains an unsafe file path."));
    }
    let mut out = create_file(&dest)?;
    let mut buf = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let n = entry
            .read(&mut buf)
            .map_err(|_| WorldError::invalid("The zip is damaged."))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > declared {
            return Err(WorldError::invalid(
                "The zip is damaged: a file is bigger than it says.",
            ));
        }
        out.write_all(buf.get(..n).unwrap_or_default())
            .map_err(|e| WorldError::io("writing a file", e))?;
    }
    out.sync_all()
        .map_err(|e| WorldError::io("writing a file", e))
}

fn sync_tree_root(staged: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(staged) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = staged;
}
