//! The on-disk part of a mod change: journal, staging, trash, and the moves
//! between them.
//!
//! Layout under the server directory (same filesystem as `data/mods`, so every
//! move is an atomic `rename`):
//!
//! ```text
//! mods-txn/journal.json   what is about to happen and how far it got
//! mods-txn/staging/       verified downloads, not yet visible to the game
//! mods-txn/trash/         jars taken out of the mods directory, kept until commit
//! ```
//!
//! The journal is written (write, fsync, rename, fsync of the directory)
//! *before* the first file moves, so after a crash there is always either no
//! journal (nothing moved) or a journal that says exactly what to undo. Undo
//! is idempotent. Nothing here reads or runs the jars.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::files::sync_dir;
use crate::db::ModChange;

pub const TXN_DIR: &str = "mods-txn";
const JOURNAL: &str = "journal.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Files are being moved; undo on recovery.
    Applying,
    /// Files are in place and the server is being restarted; undo on recovery.
    Applied,
    /// The change passed its health check; finish the records on recovery.
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub txn: String,
    pub stage: Stage,
    /// The pre-change backup this change can be rolled back to.
    pub backup_id: Option<String>,
    /// Jar names moved from staging into the mods directory.
    pub adds: Vec<String>,
    /// Jar names moved from the mods directory into the trash.
    pub removes: Vec<String>,
    /// Records to delete on commit.
    pub remove_projects: Vec<String>,
    /// Records to write on commit.
    pub upserts: Vec<ModChange>,
}

/// A step of [`apply`], offered to a hook so tests can "crash" between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Trash(String),
    Place(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("{0} already exists in the mods directory and is not ours to replace")]
    Exists(String),
    #[error("interrupted by the test hook")]
    Interrupted,
}

pub fn txn_root(server_dir: &Path) -> PathBuf {
    server_dir.join(TXN_DIR)
}

pub fn staging_dir(root: &Path) -> PathBuf {
    root.join("staging")
}

pub fn trash_dir(root: &Path) -> PathBuf {
    root.join("trash")
}

pub fn journal_path(root: &Path) -> PathBuf {
    root.join(JOURNAL)
}

/// Creates empty staging and trash directories.
pub fn begin(root: &Path) -> io::Result<()> {
    fs::create_dir_all(staging_dir(root))?;
    fs::create_dir_all(trash_dir(root))?;
    Ok(())
}

/// Durably replaces the journal.
pub fn write_journal(root: &Path, journal: &Journal) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(journal).map_err(io::Error::other)?;
    let tmp = root.join("journal.json.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, journal_path(root))?;
    sync_dir(root);
    Ok(())
}

/// The journal, if one was written. Unreadable JSON is an error (never guessed at).
pub fn read_journal(root: &Path) -> io::Result<Option<Journal>> {
    match fs::read(journal_path(root)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Moves removed jars to the trash, then staged jars into the mods directory.
///
/// `hook` sees every step before it happens; returning `false` stops with
/// [`ApplyError::Interrupted`] and leaves the disk exactly as a crash would.
pub fn apply(
    root: &Path,
    mods_dir: &Path,
    journal: &Journal,
    hook: &mut dyn FnMut(&Step) -> bool,
) -> Result<(), ApplyError> {
    fs::create_dir_all(mods_dir)?;
    for name in &journal.removes {
        if !hook(&Step::Trash(name.clone())) {
            return Err(ApplyError::Interrupted);
        }
        match fs::rename(mods_dir.join(name), trash_dir(root).join(name)) {
            Ok(()) => {}
            // Already gone (the user deleted it by hand): nothing to take out.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    for name in &journal.adds {
        if !hook(&Step::Place(name.clone())) {
            return Err(ApplyError::Interrupted);
        }
        let target = mods_dir.join(name);
        if fs::symlink_metadata(&target).is_ok() {
            return Err(ApplyError::Exists(name.clone()));
        }
        fs::rename(staging_dir(root).join(name), &target)?;
    }
    sync_dir(mods_dir);
    Ok(())
}

/// Puts the mods directory back as it was before [`apply`]. Idempotent and
/// safe to run after any partial `apply`.
pub fn undo(root: &Path, mods_dir: &Path, journal: &Journal) -> io::Result<()> {
    for name in &journal.adds {
        // Only a file that left staging was placed by us; one that is still in
        // staging means the target (if any) is somebody else's.
        // A name that is also being removed is the original coming back from
        // the trash: once restored, a second undo must leave it alone.
        let restored = journal.removes.contains(name) && !trash_dir(root).join(name).exists();
        if !staging_dir(root).join(name).exists() && !restored {
            match fs::remove_file(mods_dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    for name in &journal.removes {
        let trashed = trash_dir(root).join(name);
        if trashed.exists() {
            fs::create_dir_all(mods_dir)?;
            fs::rename(&trashed, mods_dir.join(name))?;
        }
    }
    sync_dir(mods_dir);
    Ok(())
}

/// Deletes the whole transaction directory (staging, trash, journal).
pub fn cleanup(root: &Path) -> io::Result<()> {
    match fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn listing(dir: &Path) -> io::Result<BTreeMap<String, Vec<u8>>> {
        let mut out = BTreeMap::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            out.insert(
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path())?,
            );
        }
        Ok(out)
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        mods: PathBuf,
    }

    /// mods/: old.jar, same.jar, keep.jar. Staged: new.jar, same.jar (a new
    /// build with the old name). The change removes old.jar and same.jar.
    fn fixture() -> Result<(Fixture, Journal), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let root = txn_root(dir.path());
        let mods = dir.path().join("data").join("mods");
        fs::create_dir_all(&mods)?;
        fs::write(mods.join("old.jar"), b"old")?;
        fs::write(mods.join("same.jar"), b"same-v1")?;
        fs::write(mods.join("keep.jar"), b"keep")?;
        begin(&root)?;
        fs::write(staging_dir(&root).join("new.jar"), b"new")?;
        fs::write(staging_dir(&root).join("same.jar"), b"same-v2")?;
        let journal = Journal {
            txn: "t1".into(),
            stage: Stage::Applying,
            backup_id: Some("b1".into()),
            adds: vec!["new.jar".into(), "same.jar".into()],
            removes: vec!["old.jar".into(), "same.jar".into()],
            remove_projects: vec![],
            upserts: vec![],
        };
        write_journal(&root, &journal)?;
        Ok((
            Fixture {
                _dir: dir,
                root,
                mods,
            },
            journal,
        ))
    }

    #[test]
    fn a_full_apply_then_undo_restores_the_directory() -> TestResult {
        let (f, journal) = fixture()?;
        let before = listing(&f.mods)?;
        apply(&f.root, &f.mods, &journal, &mut |_| true)?;
        let after = listing(&f.mods)?;
        assert_eq!(
            after.keys().cloned().collect::<Vec<_>>(),
            ["keep.jar", "new.jar", "same.jar"]
        );
        assert_eq!(
            after.get("same.jar").map(Vec::as_slice),
            Some(&b"same-v2"[..])
        );
        undo(&f.root, &f.mods, &journal)?;
        assert_eq!(listing(&f.mods)?, before);
        // Undo twice is the same as once.
        undo(&f.root, &f.mods, &journal)?;
        assert_eq!(listing(&f.mods)?, before);
        Ok(())
    }

    #[test]
    fn a_crash_between_any_two_steps_is_undone_exactly() -> TestResult {
        // 2 trash steps + 2 place steps: crash before each of them, and after the last.
        for crash_at in 0..=4 {
            let (f, journal) = fixture()?;
            let before = listing(&f.mods)?;
            let mut step = 0;
            let result = apply(&f.root, &f.mods, &journal, &mut |_| {
                let go = step < crash_at;
                step += 1;
                go
            });
            if crash_at < 4 {
                assert!(matches!(result, Err(ApplyError::Interrupted)), "{crash_at}");
            } else {
                assert!(result.is_ok());
            }
            // What recovery at boot does: read the journal and undo.
            let recovered = read_journal(&f.root)?.ok_or("journal missing")?;
            undo(&f.root, &f.mods, &recovered)?;
            assert_eq!(listing(&f.mods)?, before, "crash_at {crash_at}");
        }
        Ok(())
    }

    #[test]
    fn apply_refuses_to_overwrite_a_foreign_file_and_undo_keeps_it() -> TestResult {
        let (f, mut journal) = fixture()?;
        // A hand-placed new.jar the user owns.
        fs::write(f.mods.join("new.jar"), b"users own")?;
        journal.adds = vec!["new.jar".into()];
        journal.removes = vec![];
        let before = listing(&f.mods)?;
        let result = apply(&f.root, &f.mods, &journal, &mut |_| true);
        assert!(matches!(result, Err(ApplyError::Exists(_))));
        undo(&f.root, &f.mods, &journal)?;
        assert_eq!(listing(&f.mods)?, before);
        Ok(())
    }

    #[test]
    fn a_missing_jar_to_remove_is_not_an_error() -> TestResult {
        let (f, mut journal) = fixture()?;
        fs::remove_file(f.mods.join("old.jar"))?;
        journal.adds = vec![];
        journal.removes = vec!["old.jar".into()];
        apply(&f.root, &f.mods, &journal, &mut |_| true)?;
        undo(&f.root, &f.mods, &journal)?;
        assert!(!f.mods.join("old.jar").exists());
        Ok(())
    }

    #[test]
    fn journal_roundtrips_and_leaves_no_temp_file() -> TestResult {
        let (f, journal) = fixture()?;
        assert_eq!(read_journal(&f.root)?, Some(journal));
        assert!(!f.root.join("journal.json.tmp").exists());
        cleanup(&f.root)?;
        assert_eq!(read_journal(&f.root)?, None);
        cleanup(&f.root)?;
        Ok(())
    }

    #[test]
    fn a_damaged_journal_is_reported_not_guessed() -> TestResult {
        let (f, _) = fixture()?;
        fs::write(journal_path(&f.root), b"{ not json")?;
        assert!(read_journal(&f.root).is_err());
        Ok(())
    }
}
