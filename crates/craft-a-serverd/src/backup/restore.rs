//! Safe extraction of backup archives and atomic directory swap for restores.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use super::BackupError;
use super::archive::{EntryKind, Limits, copy_entry, walk};
use super::sync_dir;

/// Caps against decompression bombs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractLimits {
    /// Maximum total uncompressed file bytes.
    pub max_bytes: u64,
    /// Maximum number of entries.
    pub max_entries: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1 << 40,
            max_entries: 5_000_000,
        }
    }
}

/// Extract `archive` into `target_dir`, which must not exist (it is created).
///
/// Only regular files and directories are accepted; absolute paths, `..`, Windows prefixes,
/// backslashes, links, devices, duplicate or overlong (>1024 byte) paths are rejected, as is
/// exceeding `limits`. Nothing is ever created outside `target_dir`. On any failure after the
/// directory was created it is removed entirely. Unix modes: files 0600, dirs 0700.
pub fn extract_safely(
    archive: &Path,
    target_dir: &Path,
    limits: &ExtractLimits,
) -> Result<(), BackupError> {
    // Open first so a missing archive does not create anything.
    let file = File::open(archive)
        .map_err(|e| BackupError::io(format!("open {}", archive.display()), e))?;
    if let Some(parent) = target_dir.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| BackupError::io(format!("create {}", parent.display()), e))?;
    }
    match fs::create_dir(target_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(BackupError::AlreadyExists(target_dir.display().to_string()));
        }
        Err(e) => {
            return Err(BackupError::io(
                format!("create {}", target_dir.display()),
                e,
            ));
        }
    }
    let result = extract_into(file, target_dir, limits);
    if result.is_err() {
        let _ = fs::remove_dir_all(target_dir);
    }
    result
}

fn mkdir(path: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(path)
}

fn extract_into(file: File, target: &Path, limits: &ExtractLimits) -> Result<(), BackupError> {
    let decoder = zstd::Decoder::new(file).map_err(|e| BackupError::Corrupt(e.to_string()))?;
    let mut ar = tar::Archive::new(decoder);
    let lim = Limits {
        max_bytes: limits.max_bytes,
        max_entries: limits.max_entries,
    };
    walk(&mut ar, &lim, |e, data| {
        let mut dest = target.to_path_buf();
        dest.extend(e.rel.split('/'));
        // Components were validated; this is a belt-and-braces lexical check.
        if !dest.starts_with(target) {
            return Err(BackupError::UnsafeEntry {
                path: e.rel.clone(),
                reason: "escapes target directory",
            });
        }
        let ctx = |what: &str| format!("{what} {}", dest.display());
        match e.kind {
            EntryKind::Dir => mkdir(&dest).map_err(|er| BackupError::io(ctx("mkdir"), er)),
            EntryKind::File => {
                if let Some(parent) = dest.parent() {
                    mkdir(parent).map_err(|er| BackupError::io(ctx("mkdir"), er))?;
                }
                let mut opts = OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                let mut out = opts
                    .open(&dest)
                    .map_err(|er| BackupError::io(ctx("create"), er))?;
                if copy_entry(data, &mut out, &e.rel)? != e.size {
                    return Err(BackupError::Corrupt(format!("short entry {}", e.rel)));
                }
                out.flush()
                    .map_err(|er| BackupError::io(ctx("flush"), er))?;
                out.sync_all()
                    .map_err(|er| BackupError::io(ctx("fsync"), er))
            }
        }
    })?;
    // Drain so a truncated frame after the tar end marker is still an error.
    let mut decoder = ar.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())
        .map_err(|e| BackupError::Corrupt(e.to_string()))?;
    Ok(())
}

/// Outcome of [`recover_interrupted_swap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapRecovery {
    /// Nothing to fix.
    Nothing,
    /// `live` was missing and has been restored from `trash`.
    RestoredFromTrash,
}

/// Atomically replace `live` with `restored` using two renames.
///
/// Steps: `live -> trash`, then `restored -> live`. If the second rename fails the first is
/// rolled back (`trash -> live`). `trash` must not exist and all three paths should be on the
/// same filesystem. Crash window: a crash between the two renames leaves `live` missing and the
/// old data in `trash`; call [`recover_interrupted_swap`] at boot to move it back. After a
/// successful swap the caller may delete `trash` at leisure.
pub fn swap_into_place(live: &Path, restored: &Path, trash: &Path) -> Result<(), BackupError> {
    if trash.exists() {
        return Err(BackupError::AlreadyExists(trash.display().to_string()));
    }
    fs::rename(live, trash).map_err(|e| {
        BackupError::io(format!("move {} to {}", live.display(), trash.display()), e)
    })?;
    if let Err(e) = fs::rename(restored, live) {
        return match fs::rename(trash, live) {
            Ok(()) => Err(BackupError::io(
                format!("move {} into place (rolled back)", restored.display()),
                e,
            )),
            Err(e2) => Err(BackupError::io(
                format!(
                    "move {} into place failed ({e}) and rollback failed; old data is in {}",
                    restored.display(),
                    trash.display()
                ),
                e2,
            )),
        };
    }
    if let Some(parent) = live.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Boot-time repair after a crash inside [`swap_into_place`].
///
/// If `live` is missing and `trash` exists, moves `trash` back to `live`. If both exist the swap
/// completed (the leftover `trash` is the caller's to delete); if neither exists, nothing is done.
pub fn recover_interrupted_swap(live: &Path, trash: &Path) -> Result<SwapRecovery, BackupError> {
    if !live.exists() && trash.exists() {
        fs::rename(trash, live).map_err(|e| {
            BackupError::io(
                format!("recover {} from {}", live.display(), trash.display()),
                e,
            )
        })?;
        if let Some(parent) = live.parent() {
            sync_dir(parent)?;
        }
        return Ok(SwapRecovery::RestoredFromTrash);
    }
    Ok(SwapRecovery::Nothing)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{make_data_dir, noise, snapshot};
    use super::*;
    use std::error::Error;
    use std::path::PathBuf;

    type R = Result<(), Box<dyn Error>>;

    /// Write a tar.zst from raw (name, type, linkname, data) tuples, bypassing the tar
    /// crate's path checks.
    type RawEntry<'a> = (&'a [u8], tar::EntryType, Option<&'a [u8]>, &'a [u8]);

    fn evil(dir: &Path, entries: &[RawEntry<'_>]) -> Result<PathBuf, Box<dyn Error>> {
        let mut raw = Vec::new();
        for (name, ty, link, data) in entries {
            let mut h = tar::Header::new_old();
            {
                let old = h.as_old_mut();
                for (d, s) in old.name.iter_mut().zip(name.iter()) {
                    *d = *s;
                }
                if let Some(l) = link {
                    for (d, s) in old.linkname.iter_mut().zip(l.iter()) {
                        *d = *s;
                    }
                }
            }
            h.set_entry_type(*ty);
            h.set_mode(0o644);
            h.set_size(data.len() as u64);
            h.set_cksum();
            raw.extend_from_slice(h.as_bytes());
            raw.extend_from_slice(data);
            let pad = (512 - data.len() % 512) % 512;
            raw.extend(std::iter::repeat_n(0u8, pad));
        }
        raw.extend(std::iter::repeat_n(0u8, 1024));
        let p = dir.join("evil.tar.zst");
        fs::write(&p, zstd::encode_all(raw.as_slice(), 1)?)?;
        Ok(p)
    }

    fn assert_rejected(entries: &[RawEntry<'_>]) -> R {
        let tmp = tempfile::tempdir()?;
        let a = evil(tmp.path(), entries)?;
        let target = tmp.path().join("out");
        let r = extract_safely(&a, &target, &ExtractLimits::default());
        assert!(
            matches!(r, Err(BackupError::UnsafeEntry { .. })),
            "got {r:?}"
        );
        assert!(!target.exists(), "target left behind");
        // verify() must reject it too.
        assert!(super::super::verify(&a, None, &[]).is_err());
        Ok(())
    }

    use tar::EntryType as T;

    #[test]
    fn rejects_traversal_and_absolute() -> R {
        let ok: RawEntry<'_> = (b"good.txt", T::Regular, None, b"hi");
        assert_rejected(&[ok, (b"../escape.txt", T::Regular, None, b"x")])?;
        assert_rejected(&[(b"a/../../escape.txt", T::Regular, None, b"x")])?;
        assert_rejected(&[(b"/abs/evil.txt", T::Regular, None, b"x")])?;
        assert_rejected(&[(b"C:/evil.txt", T::Regular, None, b"x")])?;
        assert_rejected(&[(b"a\\..\\b.txt", T::Regular, None, b"x")])?;
        assert_rejected(&[(b"..", T::Directory, None, b"")])?;
        Ok(())
    }

    #[test]
    fn rejects_links_and_devices() -> R {
        assert_rejected(&[(b"ln", T::Symlink, Some(b"/etc/passwd"), b"")])?;
        assert_rejected(&[(b"ln", T::Symlink, Some(b"../.."), b"")])?;
        assert_rejected(&[
            (b"f", T::Regular, None, b"x"),
            (b"hl", T::Link, Some(b"f"), b""),
        ])?;
        assert_rejected(&[(b"dev", T::Char, None, b"")])?;
        assert_rejected(&[(b"blk", T::Block, None, b"")])?;
        assert_rejected(&[(b"fifo", T::Fifo, None, b"")])?;
        Ok(())
    }

    #[test]
    fn rejects_duplicates_and_overlong() -> R {
        assert_rejected(&[
            (b"a.txt", T::Regular, None, b"1"),
            (b"./a.txt", T::Regular, None, b"2"),
        ])?;
        assert_rejected(&[
            (b"d", T::Directory, None, b""),
            (b"d/", T::Directory, None, b""),
        ])?;
        // Overlong via GNU long name using the tar crate's builder.
        let tmp = tempfile::tempdir()?;
        let long = format!("{}/f.txt", "d".repeat(1100));
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        b.append_data(&mut h, &long, &b"x"[..])?;
        let a = tmp.path().join("long.tar.zst");
        fs::write(&a, zstd::encode_all(b.into_inner()?.as_slice(), 1)?)?;
        let target = tmp.path().join("out");
        assert!(matches!(
            extract_safely(&a, &target, &ExtractLimits::default()),
            Err(BackupError::UnsafeEntry { .. })
        ));
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    fn partial_extraction_is_cleaned_up() -> R {
        let tmp = tempfile::tempdir()?;
        let a = evil(
            tmp.path(),
            &[
                (b"fine/one.txt", T::Regular, None, b"1"),
                (b"fine/two.txt", T::Regular, None, b"2"),
                (b"x/../../bad", T::Regular, None, b"3"),
            ],
        )?;
        let target = tmp.path().join("out");
        assert!(extract_safely(&a, &target, &ExtractLimits::default()).is_err());
        assert!(!target.exists());
        assert!(!tmp.path().join("bad").exists());
        Ok(())
    }

    #[test]
    fn size_and_count_caps() -> R {
        let tmp = tempfile::tempdir()?;
        let big = noise(3, 5000);
        let a = evil(
            tmp.path(),
            &[
                (b"a", T::Regular, None, &big),
                (b"b", T::Regular, None, &big),
            ],
        )?;
        let t = tmp.path().join("out");
        let r = extract_safely(
            &a,
            &t,
            &ExtractLimits {
                max_bytes: 9999,
                max_entries: 10,
            },
        );
        assert!(
            matches!(r, Err(BackupError::TooLarge { limit: 9999 })),
            "{r:?}"
        );
        assert!(!t.exists());
        let r = extract_safely(
            &a,
            &t,
            &ExtractLimits {
                max_bytes: 10_000,
                max_entries: 1,
            },
        );
        assert!(
            matches!(r, Err(BackupError::TooManyEntries { limit: 1 })),
            "{r:?}"
        );
        assert!(!t.exists());
        extract_safely(
            &a,
            &t,
            &ExtractLimits {
                max_bytes: 10_000,
                max_entries: 2,
            },
        )?;
        assert_eq!(fs::read(t.join("b"))?, big);
        Ok(())
    }

    #[test]
    fn existing_target_is_refused_and_untouched() -> R {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("data");
        make_data_dir(&data)?;
        let info = super::super::create(&data, &tmp.path().join("b"), "n", &[])?;
        let target = tmp.path().join("exists");
        fs::create_dir(&target)?;
        fs::write(target.join("keep"), b"me")?;
        let r = extract_safely(&info.path, &target, &ExtractLimits::default());
        assert!(matches!(r, Err(BackupError::AlreadyExists(_))));
        assert_eq!(fs::read(target.join("keep"))?, b"me");
        Ok(())
    }

    #[test]
    fn truncated_archive_extract_fails_and_cleans() -> R {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("data");
        make_data_dir(&data)?;
        let info = super::super::create(&data, &tmp.path().join("b"), "n", &[])?;
        let good = fs::read(&info.path)?;
        let p = tmp.path().join("cut.tar.zst");
        fs::write(&p, good.get(..good.len() / 2).ok_or("cut")?)?;
        let t = tmp.path().join("out");
        assert!(extract_safely(&p, &t, &ExtractLimits::default()).is_err());
        assert!(!t.exists());
        // Missing archive creates nothing.
        assert!(extract_safely(&tmp.path().join("nope"), &t, &ExtractLimits::default()).is_err());
        assert!(!t.exists());
        Ok(())
    }

    #[test]
    fn extraction_matches_source_tree() -> R {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("data");
        make_data_dir(&data)?;
        let info = super::super::create(&data, &tmp.path().join("b"), "n", &[])?;
        let t = tmp.path().join("deep/er/out");
        extract_safely(&info.path, &t, &ExtractLimits::default())?;
        assert_eq!(snapshot(&data)?, snapshot(&t)?);
        Ok(())
    }

    #[test]
    fn swap_happy_path_and_recovery() -> R {
        let tmp = tempfile::tempdir()?;
        let (live, new, trash) = (
            tmp.path().join("live"),
            tmp.path().join("new"),
            tmp.path().join("trash"),
        );
        fs::create_dir(&live)?;
        fs::create_dir(&new)?;
        fs::write(live.join("f"), b"old")?;
        fs::write(new.join("f"), b"new")?;
        // Nothing to recover when live exists.
        assert_eq!(
            recover_interrupted_swap(&live, &trash)?,
            SwapRecovery::Nothing
        );
        swap_into_place(&live, &new, &trash)?;
        assert_eq!(fs::read(live.join("f"))?, b"new");
        assert_eq!(fs::read(trash.join("f"))?, b"old");
        // Completed swap with leftover trash: recovery must not touch it.
        assert_eq!(
            recover_interrupted_swap(&live, &trash)?,
            SwapRecovery::Nothing
        );
        assert_eq!(fs::read(live.join("f"))?, b"new");
        // Simulate crash between the two renames.
        fs::remove_dir_all(&live)?;
        assert_eq!(
            recover_interrupted_swap(&live, &trash)?,
            SwapRecovery::RestoredFromTrash
        );
        assert_eq!(fs::read(live.join("f"))?, b"old");
        assert!(!trash.exists());
        assert_eq!(
            recover_interrupted_swap(&live, &trash)?,
            SwapRecovery::Nothing
        );
        Ok(())
    }

    #[test]
    fn swap_rolls_back_when_second_rename_fails() -> R {
        let tmp = tempfile::tempdir()?;
        let (live, trash) = (tmp.path().join("live"), tmp.path().join("trash"));
        fs::create_dir(&live)?;
        fs::write(live.join("f"), b"old")?;
        let missing = tmp.path().join("does-not-exist");
        assert!(swap_into_place(&live, &missing, &trash).is_err());
        assert_eq!(fs::read(live.join("f"))?, b"old");
        assert!(!trash.exists());
        Ok(())
    }

    #[test]
    fn swap_refuses_existing_trash_and_missing_live() -> R {
        let tmp = tempfile::tempdir()?;
        let (live, new, trash) = (
            tmp.path().join("live"),
            tmp.path().join("new"),
            tmp.path().join("trash"),
        );
        fs::create_dir(&live)?;
        fs::create_dir(&new)?;
        fs::create_dir(&trash)?;
        assert!(matches!(
            swap_into_place(&live, &new, &trash),
            Err(BackupError::AlreadyExists(_))
        ));
        fs::remove_dir(&trash)?;
        fs::remove_dir(&live)?;
        assert!(swap_into_place(&live, &new, &trash).is_err());
        assert!(new.exists());
        Ok(())
    }
}
