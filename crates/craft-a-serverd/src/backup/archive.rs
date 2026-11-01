//! Creating and verifying `tar.zst` backup archives, plus the shared safe tar walker.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{BackupError, sync_dir};

/// zstd compression level used for backups.
const ZSTD_LEVEL: i32 = 3;
/// Maximum tar entry path length in bytes.
const MAX_PATH_BYTES: usize = 1024;
/// Maximum backup name length.
const MAX_NAME_LEN: usize = 128;

/// Result of a successful [`create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInfo {
    /// Final archive path (`<dest_dir>/<name>.tar.zst`).
    pub path: PathBuf,
    /// Size of the compressed file in bytes.
    pub size_bytes: u64,
    /// SHA-256 (hex) of the compressed bytes.
    pub sha256_hex: String,
    /// Number of files and directories archived.
    pub entries: u64,
    /// Symlinks found in the data dir and skipped.
    pub skipped_symlinks: u64,
}

/// Result of a successful [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// SHA-256 (hex) of the compressed bytes.
    pub sha256_hex: String,
    /// Size of the compressed file in bytes.
    pub size_bytes: u64,
    /// Number of entries walked.
    pub entries: u64,
    /// Total uncompressed file bytes.
    pub uncompressed_bytes: u64,
}

/// Kind of a validated tar entry (only these two are ever accepted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Dir,
}

/// A tar entry that passed path and type validation.
#[derive(Debug)]
pub(crate) struct ValidEntry {
    /// Normalized relative path using `/`, no `.`/`..`/empty parts.
    pub rel: String,
    pub kind: EntryKind,
    pub size: u64,
}

/// Caps applied while walking an archive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub max_bytes: u64,
    pub max_entries: u64,
}

/// Totals from a walk.
#[derive(Debug, Default)]
pub(crate) struct WalkStats {
    pub entries: u64,
    pub bytes: u64,
}

struct HashWriter<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if let Some(b) = buf.get(..n) {
            self.hasher.update(b);
        }
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct HashReader<R> {
    inner: R,
    hasher: Sha256,
    read: u64,
}

impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(b) = buf.get(..n) {
            self.hasher.update(b);
        }
        self.read += n as u64;
        Ok(n)
    }
}

/// Reader that yields exactly `remaining` bytes or fails (file shrank while being archived).
struct ExactReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let cap = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let Some(window) = buf.get_mut(..cap) else {
            return Ok(0);
        };
        let n = self.inner.read(window)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file shrank while being archived",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Check a backup name: 1..=128 chars of `[A-Za-z0-9._-]`, no leading or trailing dot.
fn validate_name(name: &str) -> Result<(), BackupError> {
    let bad = |why: &str| Err(BackupError::InvalidName(format!("{name:?}: {why}")));
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return bad("length must be 1..=128");
    }
    if name.starts_with('.') || name.ends_with('.') {
        return bad("must not start or end with a dot");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return bad("only ASCII letters, digits, '.', '_' and '-' are allowed");
    }
    Ok(())
}

struct Walker<'a, W: Write, H: FnMut(u64) -> io::Result<()>> {
    builder: &'a mut tar::Builder<W>,
    exclude: &'a [&'a str],
    skip_path: &'a Path,
    hook: H,
    entries: u64,
    skipped_symlinks: u64,
}

impl<W: Write, H: FnMut(u64) -> io::Result<()>> Walker<'_, W, H> {
    fn tick(&mut self) -> Result<(), BackupError> {
        (self.hook)(self.entries).map_err(|e| BackupError::io("injected failure", e))
    }

    fn add_dir(&mut self, abs: &Path, rel: &str) -> Result<(), BackupError> {
        let ctx = |what: &str, p: &Path| format!("{what} {}", p.display());
        let mut names = Vec::new();
        for e in fs::read_dir(abs).map_err(|e| BackupError::io(ctx("read dir", abs), e))? {
            let e = e.map_err(|e| BackupError::io(ctx("read dir", abs), e))?;
            names.push(e.file_name());
        }
        names.sort();
        for os in names {
            let name = os.to_str().ok_or_else(|| BackupError::UnsafeEntry {
                path: os.to_string_lossy().into_owned(),
                reason: "non-UTF-8 file name",
            })?;
            if rel.is_empty() && self.exclude.contains(&name) {
                continue;
            }
            let path = abs.join(&os);
            if path == self.skip_path {
                continue;
            }
            let rel_path = if rel.is_empty() {
                name.to_owned()
            } else {
                format!("{rel}/{name}")
            };
            let md =
                fs::symlink_metadata(&path).map_err(|e| BackupError::io(ctx("stat", &path), e))?;
            let ft = md.file_type();
            if ft.is_symlink() {
                self.skipped_symlinks += 1;
            } else if ft.is_dir() {
                self.tick()?;
                self.builder
                    .append_dir(&rel_path, &path)
                    .map_err(|e| BackupError::io(ctx("archive dir", &path), e))?;
                self.entries += 1;
                self.add_dir(&path, &rel_path)?;
            } else if ft.is_file() {
                self.tick()?;
                let file = File::open(&path).map_err(|e| BackupError::io(ctx("open", &path), e))?;
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(&md, tar::HeaderMode::Deterministic);
                header.set_size(md.len());
                let reader = ExactReader {
                    inner: file,
                    remaining: md.len(),
                };
                self.builder
                    .append_data(&mut header, &rel_path, reader)
                    .map_err(|e| BackupError::io(ctx("archive file", &path), e))?;
                self.entries += 1;
            }
            // Sockets, fifos and devices are silently ignored.
        }
        Ok(())
    }
}

/// Create `<dest_dir>/<name>.tar.zst` from `data_dir`.
///
/// Streams tar+zstd into `<name>.tar.zst.partial`, hashing the compressed bytes as they are
/// written, fsyncs, then renames atomically (and fsyncs `dest_dir` on unix). On any failure the
/// partial file is removed, so a `.tar.zst` exists only if complete. `exclude` lists top-level
/// entry names to skip. Symlinks are never followed; they are skipped and counted.
pub fn create(
    data_dir: &Path,
    dest_dir: &Path,
    name: &str,
    exclude: &[&str],
) -> Result<ArchiveInfo, BackupError> {
    create_with_hook(data_dir, dest_dir, name, exclude, |_| Ok(()))
}

/// [`create`] with a per-entry hook (receives the entry count so far); an `Err` aborts the backup.
pub(crate) fn create_with_hook(
    data_dir: &Path,
    dest_dir: &Path,
    name: &str,
    exclude: &[&str],
    hook: impl FnMut(u64) -> io::Result<()>,
) -> Result<ArchiveInfo, BackupError> {
    validate_name(name)?;
    fs::create_dir_all(dest_dir)
        .map_err(|e| BackupError::io(format!("create {}", dest_dir.display()), e))?;
    let final_path = dest_dir.join(format!("{name}.tar.zst"));
    let partial = dest_dir.join(format!("{name}.tar.zst.partial"));
    if final_path.exists() {
        return Err(BackupError::AlreadyExists(final_path.display().to_string()));
    }
    let result = write_partial(data_dir, &partial, exclude, hook).and_then(|stats| {
        fs::rename(&partial, &final_path)
            .map_err(|e| BackupError::io(format!("rename to {}", final_path.display()), e))?;
        Ok(stats)
    });
    match result {
        Ok((size_bytes, sha256_hex, entries, skipped_symlinks)) => {
            sync_dir(dest_dir)?;
            Ok(ArchiveInfo {
                path: final_path,
                size_bytes,
                sha256_hex,
                entries,
                skipped_symlinks,
            })
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Returns (size, sha256 hex, entries, skipped symlinks).
fn write_partial(
    data_dir: &Path,
    partial: &Path,
    exclude: &[&str],
    hook: impl FnMut(u64) -> io::Result<()>,
) -> Result<(u64, String, u64, u64), BackupError> {
    let io_ctx = |what: &str| format!("{what} {}", partial.display());
    let file = File::create(partial).map_err(|e| BackupError::io(io_ctx("create"), e))?;
    let hw = HashWriter {
        inner: BufWriter::new(file),
        hasher: Sha256::new(),
        written: 0,
    };
    let encoder =
        zstd::Encoder::new(hw, ZSTD_LEVEL).map_err(|e| BackupError::io(io_ctx("zstd init"), e))?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    let (entries, skipped_symlinks) = {
        let mut w = Walker {
            builder: &mut builder,
            exclude,
            skip_path: partial,
            hook,
            entries: 0,
            skipped_symlinks: 0,
        };
        w.add_dir(data_dir, "")?;
        (w.entries, w.skipped_symlinks)
    };
    let encoder = builder
        .into_inner()
        .map_err(|e| BackupError::io(io_ctx("finish tar"), e))?;
    let mut hw = encoder
        .finish()
        .map_err(|e| BackupError::io(io_ctx("finish zstd"), e))?;
    hw.flush()
        .map_err(|e| BackupError::io(io_ctx("flush"), e))?;
    let size = hw.written;
    let digest = hw.hasher.clone().finalize();
    let file = hw
        .inner
        .into_inner()
        .map_err(|e| BackupError::io(io_ctx("flush"), e.into_error()))?;
    file.sync_all()
        .map_err(|e| BackupError::io(io_ctx("fsync"), e))?;
    Ok((size, hex::encode(digest), entries, skipped_symlinks))
}

/// Normalize a raw tar path into `a/b/c`, or explain why it is unsafe.
pub(crate) fn sanitize_path(raw: &[u8]) -> Result<String, &'static str> {
    if raw.len() > MAX_PATH_BYTES {
        return Err("path too long");
    }
    let s = std::str::from_utf8(raw).map_err(|_| "non-UTF-8 path")?;
    if s.contains('\0') {
        return Err("NUL in path");
    }
    if s.contains('\\') {
        return Err("backslash in path");
    }
    if s.starts_with('/') {
        return Err("absolute path");
    }
    let mut parts = Vec::new();
    for c in s.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err("parent directory component"),
            _ if c.contains(':') => return Err("drive prefix or colon in path"),
            _ => parts.push(c),
        }
    }
    Ok(parts.join("/"))
}

fn corrupt(e: impl std::fmt::Display) -> BackupError {
    BackupError::Corrupt(e.to_string())
}

/// Walk every entry of a tar, validating type, path, duplicates and limits.
///
/// `f` must consume the entry's data (it is bounded to the declared size). The tar root entry
/// (`.`) is skipped.
pub(crate) fn walk<R: Read>(
    ar: &mut tar::Archive<R>,
    limits: &Limits,
    mut f: impl FnMut(&ValidEntry, &mut dyn Read) -> Result<(), BackupError>,
) -> Result<WalkStats, BackupError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stats = WalkStats::default();
    for entry in ar.entries().map_err(corrupt)? {
        let mut entry = entry.map_err(corrupt)?;
        let lossy = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let unsafe_entry = |reason| BackupError::UnsafeEntry {
            path: lossy.clone(),
            reason,
        };
        let kind = match entry.header().entry_type() {
            tar::EntryType::Regular => EntryKind::File,
            tar::EntryType::Directory => EntryKind::Dir,
            tar::EntryType::Symlink => return Err(unsafe_entry("symlink")),
            tar::EntryType::Link => return Err(unsafe_entry("hard link")),
            _ => return Err(unsafe_entry("unsupported entry type")),
        };
        let rel = sanitize_path(&entry.path_bytes()).map_err(unsafe_entry)?;
        if rel.is_empty() {
            if kind == EntryKind::Dir {
                continue;
            }
            return Err(unsafe_entry("empty path"));
        }
        stats.entries += 1;
        if stats.entries > limits.max_entries {
            return Err(BackupError::TooManyEntries {
                limit: limits.max_entries,
            });
        }
        let size = if kind == EntryKind::File {
            entry.size()
        } else {
            0
        };
        stats.bytes = stats
            .bytes
            .checked_add(size)
            .filter(|b| *b <= limits.max_bytes)
            .ok_or(BackupError::TooLarge {
                limit: limits.max_bytes,
            })?;
        if !seen.insert(rel.clone()) {
            return Err(unsafe_entry("duplicate path"));
        }
        f(&ValidEntry { rel, kind, size }, &mut entry)?;
    }
    Ok(stats)
}

/// Copy an entry to a writer; read failures are `Corrupt`, write failures are `Io`.
pub(crate) fn copy_entry(
    src: &mut dyn Read,
    dst: &mut dyn Write,
    what: &str,
) -> Result<u64, BackupError> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = src.read(&mut buf).map_err(corrupt)?;
        if n == 0 {
            return Ok(total);
        }
        if let Some(b) = buf.get(..n) {
            dst.write_all(b)
                .map_err(|e| BackupError::io(format!("write {what}"), e))?;
        }
        total += n as u64;
    }
}

/// Verify an archive end to end.
///
/// Reads the file once: SHA-256 is computed over the compressed bytes, the zstd stream is fully
/// decompressed and every tar entry walked (truncation or corruption fails). Unsafe entries are
/// rejected, `required_files` (relative `/` paths like `world/level.dat`) must be present as
/// regular files, and `expected_sha256` (hex, case-insensitive) must match when given.
pub fn verify(
    archive: &Path,
    expected_sha256: Option<&str>,
    required_files: &[&str],
) -> Result<VerifyReport, BackupError> {
    let file = File::open(archive)
        .map_err(|e| BackupError::io(format!("open {}", archive.display()), e))?;
    let hr = HashReader {
        inner: file,
        hasher: Sha256::new(),
        read: 0,
    };
    let decoder = zstd::Decoder::new(hr).map_err(corrupt)?;
    let mut ar = tar::Archive::new(decoder);
    let limits = Limits {
        max_bytes: u64::MAX,
        max_entries: u64::MAX,
    };
    let mut files: HashSet<String> = HashSet::new();
    let stats = walk(&mut ar, &limits, |e, data| {
        if e.kind == EntryKind::File {
            files.insert(e.rel.clone());
        }
        copy_entry(data, &mut io::sink(), "verify").map(|_| ())
    })?;
    // Drain the rest of the zstd stream and the file so the hash covers every byte and
    // truncation after the tar end marker is still caught.
    let mut decoder = ar.into_inner();
    io::copy(&mut decoder, &mut io::sink()).map_err(corrupt)?;
    let mut rest = decoder.finish();
    io::copy(&mut rest, &mut io::sink()).map_err(corrupt)?;
    let hr = rest.into_inner();
    let actual = hex::encode(hr.hasher.finalize());
    if let Some(exp) = expected_sha256
        && !exp.eq_ignore_ascii_case(&actual)
    {
        return Err(BackupError::ChecksumMismatch {
            expected: exp.to_owned(),
            actual,
        });
    }
    for req in required_files {
        let norm = req.trim_matches('/');
        if !files.contains(norm) {
            return Err(BackupError::MissingRequired((*req).to_owned()));
        }
    }
    Ok(VerifyReport {
        sha256_hex: actual,
        size_bytes: hr.read,
        entries: stats.entries,
        uncompressed_bytes: stats.bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{make_data_dir, noise};
    use super::*;
    use std::error::Error;

    type R = Result<(), Box<dyn Error>>;

    fn setup() -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("data");
        make_data_dir(&data)?;
        Ok((tmp, data))
    }

    fn list(dir: &Path) -> Result<Vec<String>, Box<dyn Error>> {
        let mut v = Vec::new();
        for e in fs::read_dir(dir)? {
            v.push(e?.file_name().to_string_lossy().into_owned());
        }
        v.sort();
        Ok(v)
    }

    #[test]
    fn name_validation() {
        for ok in ["a", "2026-11-01_daily.1", "A-b_c"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        let long = "a".repeat(129);
        for bad in [
            "", ".hidden", "a/b", "a\\b", "..", "x.", "a b", "a:b", "é", "a\0b", &long,
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn create_excludes_and_counts() -> R {
        let (tmp, data) = setup()?;
        let info = create(&data, &tmp.path().join("out"), "n", &["logs"])?;
        // world, world/region, world/empty, world/level.dat, 4 mca, mods, jar, props, zero.bin
        assert_eq!(info.entries, 12);
        assert_eq!(info.skipped_symlinks, 0);
        assert_eq!(list(&tmp.path().join("out"))?, vec!["n.tar.zst"]);
        let rep = verify(&info.path, None, &[])?;
        assert_eq!(rep.entries, 12);
        Ok(())
    }

    #[test]
    fn rejects_bad_name_and_existing_target() -> R {
        let (tmp, data) = setup()?;
        let out = tmp.path().join("out");
        assert!(matches!(
            create(&data, &out, "../x", &[]),
            Err(BackupError::InvalidName(_))
        ));
        create(&data, &out, "dup", &[])?;
        assert!(matches!(
            create(&data, &out, "dup", &[]),
            Err(BackupError::AlreadyExists(_))
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped() -> R {
        let (tmp, data) = setup()?;
        std::os::unix::fs::symlink("/etc/passwd", data.join("evil"))?;
        std::os::unix::fs::symlink(&data, data.join("world/loop"))?;
        let info = create(&data, &tmp.path().join("out"), "n", &[])?;
        assert_eq!(info.skipped_symlinks, 2);
        verify(&info.path, None, &[])?;
        Ok(())
    }

    #[test]
    fn killed_midway_leaves_no_archive() -> R {
        let (tmp, data) = setup()?;
        let out = tmp.path().join("out");
        for fail_at in 0..12u64 {
            let mut hook = |n: u64| {
                if n == fail_at {
                    Err(io::Error::other("simulated kill"))
                } else {
                    Ok(())
                }
            };
            let r = create_with_hook(&data, &out, "n", &[], &mut hook);
            assert!(r.is_err(), "fail_at={fail_at}");
            assert_eq!(list(&out)?, Vec::<String>::new(), "fail_at={fail_at}");
        }
        Ok(())
    }

    #[test]
    fn rename_failure_removes_partial() -> R {
        let (tmp, data) = setup()?;
        let out = tmp.path().join("out");
        // A leftover *file* named like the partial is overwritten; make the final path
        // appear as a directory only after the existence check via the hook.
        let final_path = out.join("n.tar.zst");
        let fp = final_path.clone();
        let mut hook = move |n: u64| {
            if n == 3 {
                fs::create_dir_all(&fp)?;
                fs::write(fp.join("blocker"), b"x")?;
            }
            Ok(())
        };
        let r = create_with_hook(&data, &out, "n", &[], &mut hook);
        assert!(r.is_err());
        assert!(final_path.is_dir());
        assert!(!out.join("n.tar.zst.partial").exists());
        // The directory is not a verifiable archive.
        assert!(verify(&final_path, None, &[]).is_err());
        Ok(())
    }

    #[test]
    fn verify_detects_missing_required_and_bad_checksum() -> R {
        let (tmp, data) = setup()?;
        let info = create(&data, &tmp.path().join("out"), "n", &["logs"])?;
        assert!(matches!(
            verify(&info.path, None, &["world/nope.dat"]),
            Err(BackupError::MissingRequired(_))
        ));
        // A directory is not a required *file*.
        assert!(matches!(
            verify(&info.path, None, &["world/region"]),
            Err(BackupError::MissingRequired(_))
        ));
        assert!(matches!(
            verify(&info.path, Some(&"0".repeat(64)), &[]),
            Err(BackupError::ChecksumMismatch { .. })
        ));
        verify(
            &info.path,
            Some(&info.sha256_hex.to_uppercase()),
            &["world/level.dat", "mods/fabric-api.jar"],
        )?;
        Ok(())
    }

    #[test]
    fn corrupted_bytes_fail_verify() -> R {
        let (tmp, data) = setup()?;
        let info = create(&data, &tmp.path().join("out"), "n", &[])?;
        let good = fs::read(&info.path)?;
        let step = (good.len() / 40).max(1);
        for pos in (0..good.len()).step_by(step) {
            let mut bad = good.clone();
            if let Some(b) = bad.get_mut(pos) {
                *b ^= 0x40;
            }
            let p = tmp.path().join("bad.tar.zst");
            fs::write(&p, &bad)?;
            // Even if decompression happened to succeed, the checksum must catch it.
            assert!(
                verify(&p, Some(&info.sha256_hex), &[]).is_err(),
                "pos={pos}"
            );
        }
        Ok(())
    }

    #[test]
    fn truncated_archives_fail_verify_even_without_checksum() -> R {
        let (tmp, data) = setup()?;
        let info = create(&data, &tmp.path().join("out"), "n", &[])?;
        let good = fs::read(&info.path)?;
        let p = tmp.path().join("t.tar.zst");
        for cut in [0, 1, 10, good.len() / 2, good.len() - 1] {
            fs::write(&p, good.get(..cut).ok_or("cut")?)?;
            assert!(verify(&p, None, &[]).is_err(), "cut={cut}");
        }
        // Garbage appended after a valid stream is rejected too.
        let mut extra = good.clone();
        extra.extend_from_slice(&noise(5, 100));
        fs::write(&p, &extra)?;
        assert!(verify(&p, None, &[]).is_err());
        Ok(())
    }

    #[test]
    fn not_an_archive_fails() -> R {
        let tmp = tempfile::tempdir()?;
        let p = tmp.path().join("x.tar.zst");
        fs::write(&p, b"definitely not zstd")?;
        assert!(matches!(
            verify(&p, None, &[]),
            Err(BackupError::Corrupt(_))
        ));
        assert!(matches!(
            verify(&tmp.path().join("missing"), None, &[]),
            Err(BackupError::Io { .. })
        ));
        Ok(())
    }

    #[test]
    fn sanitize_cases() {
        assert_eq!(sanitize_path(b"a/b/c").as_deref(), Ok("a/b/c"));
        assert_eq!(sanitize_path(b"./a//b/").as_deref(), Ok("a/b"));
        assert_eq!(sanitize_path(b".").as_deref(), Ok(""));
        for bad in [
            &b"/etc/passwd"[..],
            b"../x",
            b"a/../b",
            b"a/..",
            b"C:/x",
            b"C:\\x",
            b"a\\b",
            b"a\0b",
            b"\xff\xfe",
        ] {
            assert!(sanitize_path(bad).is_err(), "{bad:?}");
        }
        assert!(sanitize_path(&vec![b'a'; 1025]).is_err());
        assert!(sanitize_path(&vec![b'a'; 1024]).is_ok());
    }
}
