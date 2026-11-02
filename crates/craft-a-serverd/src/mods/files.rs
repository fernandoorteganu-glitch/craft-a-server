//! File-level helpers for mods: safe names, a hashing writer, a structural
//! jar check, and the inventory of the mods directory.
//!
//! A mod jar is untrusted code. Nothing here executes, loads or unpacks it:
//! the check only reads the zip's central directory to see that it is a zip
//! that names a `fabric.mod.json`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha512};

/// Largest jar the daemon will download (the biggest popular mods are a few
/// tens of MiB).
pub const MAX_MOD_BYTES: u64 = 256 * 1024 * 1024;

const MAX_NAME_CHARS: usize = 120;
const EOCD_SIGNATURE: [u8; 4] = [b'P', b'K', 5, 6];
const CENTRAL_SIGNATURE: [u8; 4] = [b'P', b'K', 1, 2];
const LOCAL_SIGNATURE: [u8; 4] = [b'P', b'K', 3, 4];
/// The zip end-of-central-directory record is 22 bytes plus a comment of up to 64 KiB.
const EOCD_SEARCH: u64 = 22 + 65_535;
const MAX_CENTRAL_DIRECTORY: u64 = 8 * 1024 * 1024;

/// The file name to store a mod under.
///
/// Modrinth's `filename` is used only when it is plain: `[A-Za-z0-9._+-]`,
/// ends in `.jar`, does not start with a dot and contains no `..`. Anything
/// else (path separators, traversal, spaces, control characters, a wrong
/// extension) is replaced by `<project id>-<version id>.jar`, which is safe
/// because both ids are validated.
pub fn sanitize_jar_name(raw: &str, project_id: &str, version_id: &str) -> String {
    let plain = !raw.is_empty()
        && raw.len() <= MAX_NAME_CHARS
        && !raw.starts_with('.')
        && !raw.contains("..")
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
        && raw.len() > ".jar".len()
        && raw
            .get(raw.len() - ".jar".len()..)
            .is_some_and(|ext| ext.eq_ignore_ascii_case(".jar"));
    if plain {
        raw.to_owned()
    } else {
        format!("{project_id}-{version_id}.jar")
    }
}

/// True for a name this module would produce: safe to join onto the mods directory.
pub fn is_safe_jar_name(name: &str) -> bool {
    sanitize_jar_name(name, "x", "y") == name || name == "x-y.jar"
}

/// A file writer that hashes what it writes and refuses to exceed `max` bytes.
#[derive(Debug)]
pub struct HashingFile {
    file: File,
    hasher: Sha512,
    written: u64,
    max: u64,
}

impl HashingFile {
    pub fn create(path: &Path, max: u64) -> io::Result<HashingFile> {
        Ok(HashingFile {
            file: File::create(path)?,
            hasher: Sha512::new(),
            written: 0,
            max,
        })
    }

    /// Flushes to disk and returns the lowercase hex SHA-512 and the byte count.
    pub fn finish(mut self) -> io::Result<(String, u64)> {
        self.file.flush()?;
        self.file.sync_all()?;
        Ok((hex::encode(self.hasher.finalize()), self.written))
    }
}

impl Write for HashingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(buf.len() as u64) > self.max {
            return Err(io::Error::other("download over the size limit"));
        }
        self.file.write_all(buf)?;
        self.hasher.update(buf);
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// SHA-512 of a file, as lowercase hex.
pub fn sha512_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
}

/// Checks that `path` is a zip archive that lists `fabric.mod.json`.
///
/// Reads only the end-of-central-directory record and the directory itself.
/// Archives that use zip64 markers are accepted on the magic number alone
/// (their directory is not parsed).
pub fn check_fabric_jar(path: &Path) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("cannot be opened: {e}"))?;
    let len = file
        .metadata()
        .map_err(|e| format!("cannot be inspected: {e}"))?
        .len();
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .map_err(|_| "it is not a zip archive".to_owned())?;
    if magic != LOCAL_SIGNATURE {
        return Err("it is not a zip archive".to_owned());
    }
    let tail_len = len.min(EOCD_SEARCH);
    file.seek(SeekFrom::Start(len - tail_len))
        .map_err(|e| format!("cannot be read: {e}"))?;
    let mut tail = vec![0_u8; usize::try_from(tail_len).map_err(|_| "too large".to_owned())?];
    file.read_exact(&mut tail)
        .map_err(|e| format!("cannot be read: {e}"))?;
    let eocd = tail
        .windows(4)
        .rposition(|w| w == EOCD_SIGNATURE)
        .ok_or_else(|| "its zip directory is missing or damaged".to_owned())?;
    let record = tail
        .get(eocd..eocd + 22)
        .ok_or_else(|| "its zip directory is truncated".to_owned())?;
    let entries = u64::from(u16_at(record, 10));
    let dir_size = u64::from(u32_at(record, 12));
    let dir_offset = u64::from(u32_at(record, 16));
    if entries == 0xFFFF || dir_size == 0xFFFF_FFFF || dir_offset == 0xFFFF_FFFF {
        return Ok(());
    }
    if dir_size > MAX_CENTRAL_DIRECTORY || dir_offset.saturating_add(dir_size) > len {
        return Err("its zip directory is damaged".to_owned());
    }
    file.seek(SeekFrom::Start(dir_offset))
        .map_err(|e| format!("cannot be read: {e}"))?;
    let mut dir = vec![0_u8; usize::try_from(dir_size).map_err(|_| "too large".to_owned())?];
    file.read_exact(&mut dir)
        .map_err(|e| format!("cannot be read: {e}"))?;
    if central_names(&dir, entries)?
        .iter()
        .any(|n| n.as_slice() == b"fabric.mod.json")
    {
        Ok(())
    } else {
        Err("it has no fabric.mod.json, so it is not a Fabric mod".to_owned())
    }
}

fn u16_at(buf: &[u8], at: usize) -> u16 {
    buf.get(at..at + 2)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map_or(0, u16::from_le_bytes)
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    buf.get(at..at + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map_or(0, u32::from_le_bytes)
}

/// File names listed in a zip central directory.
fn central_names(dir: &[u8], entries: u64) -> Result<Vec<Vec<u8>>, String> {
    let mut names = Vec::new();
    let mut at = 0_usize;
    for _ in 0..entries {
        let head = dir
            .get(at..at + 46)
            .ok_or_else(|| "its zip directory is truncated".to_owned())?;
        if head.get(..4) != Some(&CENTRAL_SIGNATURE[..]) {
            return Err("its zip directory is damaged".to_owned());
        }
        let name_len = usize::from(u16_at(head, 28));
        let extra_len = usize::from(u16_at(head, 30));
        let comment_len = usize::from(u16_at(head, 32));
        let name = dir
            .get(at + 46..at + 46 + name_len)
            .ok_or_else(|| "its zip directory is truncated".to_owned())?;
        names.push(name.to_vec());
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(names)
}

/// One `.jar` found in a mods directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryEntry {
    pub file_name: String,
    pub size_bytes: u64,
}

/// Lists the regular `.jar` files directly inside `mods_dir`, sorted by name.
/// Symlinks, directories and other files are not mods and are skipped. A
/// missing directory is an empty inventory.
pub fn inventory(mods_dir: &Path) -> io::Result<Vec<InventoryEntry>> {
    const MAX_ENTRIES: usize = 5000;
    let entries = match std::fs::read_dir(mods_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let meta = entry.file_type()?;
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.to_ascii_lowercase().ends_with(".jar") {
            continue;
        }
        out.push(InventoryEntry {
            file_name: name,
            size_bytes: entry.metadata()?.len(),
        });
        if out.len() >= MAX_ENTRIES {
            break;
        }
    }
    out.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(out)
}

/// Syncs a directory so renames inside it survive a power cut (no-op where
/// directories cannot be opened).
pub fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A tiny zip writer (stored entries) so tests can make jars.

    pub fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            out.extend_from_slice(&[b'P', b'K', 3, 4, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&0_u32.to_le_bytes()); // crc (not checked)
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0_u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&[b'P', b'K', 1, 2, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&0_u32.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let dir_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&[b'P', b'K', 5, 6, 0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&dir_offset.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out
    }

    /// A jar that passes `check_fabric_jar`.
    pub fn fabric_jar(extra: &[u8]) -> Vec<u8> {
        zip(&[("fabric.mod.json", b"{}"), ("data.bin", extra)])
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{fabric_jar, zip};
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn plain_names_are_kept() {
        for ok in [
            "fabric-api-0.165.0+26.3.jar",
            "Jade-mc26.3-Fabric-26.3.5.jar",
            "a_b-1.JAR",
        ] {
            assert_eq!(sanitize_jar_name(ok, "PPPPPPPP", "VVVVVVVV"), ok);
        }
    }

    #[test]
    fn hostile_names_become_the_id_based_name() {
        for bad in [
            "../x.jar",
            "..\\x.jar",
            "/etc/cron.d/x.jar",
            "a/b.jar",
            "C:\\x.jar",
            "x.jar/",
            "x.jar\0",
            "x.txt",
            "x.jar.exe",
            ".hidden.jar",
            "a..b.jar",
            "sp ace.jar",
            "new\nline.jar",
            ".jar",
            "",
            &format!("{}.jar", "a".repeat(200)),
            "\u{202e}rab.jar",
        ] {
            let got = sanitize_jar_name(bad, "PPPPPPPP", "VVVVVVVV");
            assert_eq!(got, "PPPPPPPP-VVVVVVVV.jar", "{bad:?}");
            assert!(is_safe_jar_name(&got));
        }
    }

    #[test]
    fn safe_name_check_matches_the_sanitizer() {
        assert!(is_safe_jar_name("fabric-api-1.0.jar"));
        assert!(!is_safe_jar_name("../x.jar"));
        assert!(!is_safe_jar_name("a/b.jar"));
        assert!(!is_safe_jar_name("x.exe"));
    }

    #[test]
    fn hashing_writer_hashes_counts_and_caps() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("f");
        let mut w = HashingFile::create(&path, 10)?;
        w.write_all(b"abc")?;
        let (hash, n) = w.finish()?;
        assert_eq!(n, 3);
        assert_eq!(hash, hex::encode(Sha512::digest(b"abc")));
        assert_eq!(sha512_file(&path)?, hash);

        let mut w = HashingFile::create(&path, 4)?;
        w.write_all(b"abcd")?;
        assert!(w.write_all(b"e").is_err());
        Ok(())
    }

    #[test]
    fn jar_check_accepts_a_fabric_jar_and_rejects_the_rest() -> TestResult {
        let dir = tempfile::tempdir()?;
        let write = |name: &str, bytes: &[u8]| -> io::Result<std::path::PathBuf> {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes)?;
            Ok(p)
        };
        assert_eq!(
            check_fabric_jar(&write("ok.jar", &fabric_jar(b"x"))?),
            Ok(())
        );
        let forge = zip(&[("META-INF/mods.toml", b"x")]);
        let err = check_fabric_jar(&write("forge.jar", &forge)?)
            .err()
            .unwrap_or_default();
        assert!(err.contains("fabric.mod.json"), "{err}");
        for (name, bytes) in [
            ("text.jar", b"hello world, not a zip".to_vec()),
            ("empty.jar", Vec::new()),
            ("magic.jar", b"PK\x03\x04 and then nothing useful".to_vec()),
        ] {
            assert!(check_fabric_jar(&write(name, &bytes)?).is_err(), "{name}");
        }
        // A truncated real jar loses its directory.
        let mut cut = fabric_jar(b"x");
        cut.truncate(cut.len() - 30);
        assert!(check_fabric_jar(&write("cut.jar", &cut)?).is_err());
        Ok(())
    }

    #[test]
    fn inventory_lists_only_regular_jars() -> TestResult {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("b.jar"), b"12")?;
        std::fs::write(dir.path().join("a.JAR"), b"1")?;
        std::fs::write(dir.path().join("notes.txt"), b"x")?;
        std::fs::create_dir(dir.path().join("sub.jar"))?;
        let inv = inventory(dir.path())?;
        assert_eq!(
            inv,
            vec![
                InventoryEntry {
                    file_name: "a.JAR".into(),
                    size_bytes: 1
                },
                InventoryEntry {
                    file_name: "b.jar".into(),
                    size_bytes: 2
                },
            ]
        );
        assert!(inventory(&dir.path().join("missing"))?.is_empty());
        Ok(())
    }
}
