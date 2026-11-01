//! Backups: archive creation/verification/restore and retention. (Phase 1)
//!
//! All APIs are synchronous `std::fs` code; async callers wrap them in `spawn_blocking`.
//!
//! - [`archive::create`]: stream a data dir into `<name>.tar.zst.partial`, hash while writing,
//!   fsync, then atomically rename. A non-`.partial` archive never exists unless it is complete.
//! - [`archive::verify`]: read the whole archive once, check SHA-256, decompress and walk every
//!   tar entry, check required files, reject unsafe entries.
//! - [`restore::extract_safely`]: extract into a fresh dir with path/type/size validation;
//!   [`restore::swap_into_place`] and [`restore::recover_interrupted_swap`] replace the live dir.
//! - [`retention::select_deletions`]: pure retention policy over backup metadata.
//! - [`space_ok`]: pure free-space gate (backups blocked below 15 % free).

pub mod archive;
pub mod restore;
pub mod retention;

use std::io;
use std::path::Path;

pub use archive::{ArchiveInfo, VerifyReport, create, verify};
pub use restore::{
    ExtractLimits, SwapRecovery, extract_safely, recover_interrupted_swap, swap_into_place,
};
pub use retention::{BackupKind, BackupMeta, RetentionPolicy, select_deletions};

/// Minimum free-space fraction required to start a backup (spec: 15 %).
pub const MIN_FREE_FRACTION: f64 = 0.15;

/// Everything that can go wrong in the backup module.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// Backup name failed validation.
    #[error("invalid backup name: {0}")]
    InvalidName(String),
    /// An I/O operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The archive is truncated, corrupt or not a valid tar.zst.
    #[error("corrupt archive: {0}")]
    Corrupt(String),
    /// The compressed bytes do not match the expected checksum.
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// Expected hex digest.
        expected: String,
        /// Computed hex digest.
        actual: String,
    },
    /// A file that must exist in the archive is missing.
    #[error("required file missing from archive: {0}")]
    MissingRequired(String),
    /// An entry is dangerous (traversal, link, device, bad path...).
    #[error("unsafe archive entry {path:?}: {reason}")]
    UnsafeEntry {
        /// Entry path (lossy).
        path: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// Uncompressed size exceeds the configured cap.
    #[error("archive exceeds the {limit} byte uncompressed limit")]
    TooLarge {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// Entry count exceeds the configured cap.
    #[error("archive exceeds the {limit} entry limit")]
    TooManyEntries {
        /// The limit that was exceeded.
        limit: u64,
    },
    /// A destination that must not exist already does.
    #[error("destination already exists: {0}")]
    AlreadyExists(String),
    /// Not enough free disk space to take a backup.
    #[error("insufficient free space for backup")]
    InsufficientSpace,
}

impl BackupError {
    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// True when free space is at least `min_free_fraction` of the total.
///
/// Returns false for a zero total or non-finite/out-of-range fractions (fail closed).
pub fn space_ok(free_bytes: u64, total_bytes: u64, min_free_fraction: f64) -> bool {
    if total_bytes == 0
        || !min_free_fraction.is_finite()
        || !(0.0..=1.0).contains(&min_free_fraction)
    {
        return false;
    }
    (free_bytes as f64) / (total_bytes as f64) >= min_free_fraction
}

/// fsync a directory so renames inside it are durable (no-op off unix).
pub(crate) fn sync_dir(dir: &Path) -> Result<(), BackupError> {
    #[cfg(unix)]
    {
        let f = std::fs::File::open(dir)
            .map_err(|e| BackupError::io(format!("open dir {}", dir.display()), e))?;
        f.sync_all()
            .map_err(|e| BackupError::io(format!("fsync dir {}", dir.display()), e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    /// Deterministic pseudo-random bytes (xorshift), good enough as incompressible filler.
    pub fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed | 1;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            out.extend_from_slice(&s.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Build a fake Minecraft data dir.
    pub fn make_data_dir(root: &Path) -> std::io::Result<()> {
        fs::create_dir_all(root.join("world/region"))?;
        fs::create_dir_all(root.join("mods"))?;
        fs::create_dir_all(root.join("logs"))?;
        fs::create_dir_all(root.join("world/empty"))?;
        fs::write(root.join("world/level.dat"), noise(1, 2048))?;
        for i in 0..4u64 {
            fs::write(
                root.join(format!("world/region/r.{i}.0.mca")),
                noise(10 + i, 20_000 + usize::try_from(i).unwrap_or(0) * 777),
            )?;
        }
        fs::write(root.join("mods/fabric-api.jar"), noise(99, 5000))?;
        fs::write(root.join("server.properties"), b"level-name=world\n")?;
        fs::write(root.join("zero.bin"), b"")?;
        fs::write(root.join("logs/latest.log"), b"hello log\n")?;
        Ok(())
    }

    /// Map of relative path -> contents (None for dirs).
    pub fn snapshot(root: &Path) -> std::io::Result<BTreeMap<String, Option<Vec<u8>>>> {
        fn go(
            base: &Path,
            dir: &Path,
            out: &mut BTreeMap<String, Option<Vec<u8>>>,
        ) -> std::io::Result<()> {
            for e in fs::read_dir(dir)? {
                let e = e?;
                let p = e.path();
                let rel = p
                    .strip_prefix(base)
                    .map_err(std::io::Error::other)?
                    .to_string_lossy()
                    .replace('\\', "/");
                if e.file_type()?.is_dir() {
                    out.insert(rel, None);
                    go(base, &p, out)?;
                } else {
                    out.insert(rel, Some(fs::read(&p)?));
                }
            }
            Ok(())
        }
        let mut out = BTreeMap::new();
        go(root, root, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{make_data_dir, snapshot};
    use super::*;
    use std::error::Error;
    use std::fs;

    type R = Result<(), Box<dyn Error>>;

    #[test]
    fn space_ok_edges() {
        assert!(space_ok(15, 100, MIN_FREE_FRACTION));
        assert!(!space_ok(14, 100, MIN_FREE_FRACTION));
        assert!(space_ok(100, 100, 1.0));
        assert!(space_ok(0, 100, 0.0));
        assert!(!space_ok(0, 0, 0.15));
        assert!(!space_ok(10, 100, f64::NAN));
        assert!(!space_ok(10, 100, -0.1));
        assert!(!space_ok(10, 100, 1.5));
        assert!(space_ok(u64::MAX, u64::MAX, 0.15));
    }

    #[test]
    fn full_round_trip() -> R {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("data");
        make_data_dir(&data)?;
        let dest = tmp.path().join("backups");
        let info = create(&data, &dest, "b1", &["logs", "crash-reports", "cache"])?;
        assert!(info.path.ends_with("b1.tar.zst"));
        assert!(!dest.join("b1.tar.zst.partial").exists());
        let rep = verify(&info.path, Some(&info.sha256_hex), &["world/level.dat"])?;
        assert_eq!(rep.sha256_hex, info.sha256_hex);
        assert_eq!(rep.size_bytes, info.size_bytes);
        let out = tmp.path().join("restored");
        extract_safely(&info.path, &out, &ExtractLimits::default())?;
        let mut want = snapshot(&data)?;
        want.retain(|k, _| k != "logs" && !k.starts_with("logs/"));
        assert_eq!(want, snapshot(&out)?);
        Ok(())
    }

    #[test]
    fn restore_then_swap_replaces_live() -> R {
        let tmp = tempfile::tempdir()?;
        let live = tmp.path().join("live");
        make_data_dir(&live)?;
        let info = create(&live, &tmp.path().join("b"), "x", &[])?;
        fs::write(live.join("world/level.dat"), b"damaged")?;
        let restored = tmp.path().join("restored");
        extract_safely(&info.path, &restored, &ExtractLimits::default())?;
        swap_into_place(&live, &restored, &tmp.path().join("trash"))?;
        assert_ne!(fs::read(live.join("world/level.dat"))?, b"damaged");
        assert_eq!(
            fs::read(tmp.path().join("trash/world/level.dat"))?,
            b"damaged"
        );
        assert!(!restored.exists());
        Ok(())
    }
}
