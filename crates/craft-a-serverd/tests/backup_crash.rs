//! Phase 1 exit criterion: a world can be restored after the process is
//! killed in the middle of a backup.
//!
//! The test re-runs its own binary as a child that takes backups in a loop,
//! SIGKILLs it at random moments, and then checks the invariants that make
//! backups safe: only complete archives ever carry the final name, every such
//! archive verifies, and the newest one restores byte for byte.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use craft_a_serverd::backup::archive;
use craft_a_serverd::backup::restore::{self, ExtractLimits};
use rand::RngExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const CHILD_ENV: &str = "CRAFT_CRASH_CHILD";
const DATA_ENV: &str = "CRAFT_CRASH_DATA";
const DEST_ENV: &str = "CRAFT_CRASH_DEST";
const PREFIX_ENV: &str = "CRAFT_CRASH_PREFIX";
const ROUNDS: usize = 30;

/// Debug builds compress and hash far slower, so they get a smaller world.
fn chunk_bytes() -> usize {
    if cfg!(debug_assertions) {
        512 * 1024
    } else {
        8 * 1024 * 1024
    }
}

/// A world big enough that a backup takes a few hundred milliseconds.
fn make_world(root: &Path) -> TestResult {
    let mut rng = rand::rng();
    fs::create_dir_all(root.join("world/region"))?;
    fs::create_dir_all(root.join("mods"))?;
    fs::write(root.join("world/level.dat"), b"level-data")?;
    for i in 0..24 {
        let mut chunk = vec![0u8; chunk_bytes()];
        rng.fill(&mut chunk[..]);
        let mut f = fs::File::create(root.join(format!("world/region/r.{i}.0.mca")))?;
        f.write_all(&chunk)?;
    }
    fs::write(root.join("mods/some-mod.jar"), vec![7u8; 4096])?;
    Ok(())
}

/// Child mode: back up forever until killed.
#[test]
fn child_backup_loop() -> TestResult {
    if std::env::var_os(CHILD_ENV).is_none() {
        return Ok(()); // a normal test run: nothing to do
    }
    let data = PathBuf::from(std::env::var(DATA_ENV)?);
    let dest = PathBuf::from(std::env::var(DEST_ENV)?);
    let prefix = std::env::var(PREFIX_ENV)?;
    for i in 0u64.. {
        archive::create(&data, &dest, &format!("{prefix}-{i}"), &["logs"])?;
    }
    Ok(())
}

fn spawn_child(data: &Path, dest: &Path, prefix: &str) -> std::io::Result<std::process::Child> {
    Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "child_backup_loop",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env(DATA_ENV, data)
        .env(DEST_ENV, dest)
        .env(PREFIX_ENV, prefix)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn trees_equal(a: &Path, b: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let mut names_a: Vec<_> = fs::read_dir(a)?.collect::<Result<Vec<_>, _>>()?;
    let mut names_b: Vec<_> = fs::read_dir(b)?.collect::<Result<Vec<_>, _>>()?;
    names_a.sort_by_key(|e| e.file_name());
    names_b.sort_by_key(|e| e.file_name());
    if names_a.len() != names_b.len() {
        return Ok(false);
    }
    for (ea, eb) in names_a.iter().zip(&names_b) {
        if ea.file_name() != eb.file_name() {
            return Ok(false);
        }
        let (ta, tb) = (ea.file_type()?, eb.file_type()?);
        if ta.is_dir() != tb.is_dir() {
            return Ok(false);
        }
        let same = if ta.is_dir() {
            trees_equal(&ea.path(), &eb.path())?
        } else {
            fs::read(ea.path())? == fs::read(eb.path())?
        };
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

#[test]
fn killing_a_backup_midway_never_corrupts_or_loses_backups() -> TestResult {
    if std::env::var_os(CHILD_ENV).is_some() {
        return Ok(());
    }
    let tmp = tempfile::tempdir()?;
    let data = tmp.path().join("data");
    let dest = tmp.path().join("backups");
    fs::create_dir_all(&dest)?;
    make_world(&data)?;

    // Calibrate: how long does one uninterrupted backup take on this machine?
    // Kill times are drawn around that, so both outcomes (killed mid-write,
    // finished in time) happen regardless of build profile or disk speed.
    let probe_dest = tmp.path().join("probe");
    fs::create_dir_all(&probe_dest)?;
    let started = std::time::Instant::now();
    archive::create(&data, &probe_dest, "probe", &[])?;
    let one_backup = started.elapsed();
    fs::remove_dir_all(&probe_dest)?;
    let (low, high) = (
        one_backup.as_millis() as u64 / 5 + 1,
        one_backup.as_millis() as u64 * 5 / 2 + 2,
    );

    let mut rng = rand::rng();
    let mut kills_with_partial = 0;
    let mut finals_seen = 0;

    for round in 0..ROUNDS {
        let mut child = spawn_child(&data, &dest, &format!("r{round}"))?;
        std::thread::sleep(Duration::from_millis(rng.random_range(low..high)));
        child.kill()?; // SIGKILL on unix: no cleanup code runs
        child.wait()?;

        let mut finals = Vec::new();
        for entry in fs::read_dir(&dest)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.ends_with(".partial") {
                kills_with_partial += 1;
                continue; // allowed: reconciliation deletes these at boot
            }
            assert!(name.ends_with(".tar.zst"), "unexpected file {name}");
            finals.push(path);
        }

        // Every archive that has its final name must be complete and intact.
        for path in &finals {
            archive::verify(path, None, &["world/level.dat"])
                .map_err(|e| format!("round {round}: {} failed verify: {e}", path.display()))?;
        }
        finals_seen += finals.len();

        // And one of them must restore to exactly the source tree.
        if let Some(newest) = finals
            .iter()
            .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
        {
            let target = tmp.path().join(format!("restored-{round}"));
            restore::extract_safely(newest, &target, &ExtractLimits::default())?;
            assert!(
                trees_equal(&data, &target)?,
                "round {round}: restored tree differs"
            );
            fs::remove_dir_all(&target)?;
        }

        // Boot cleanup, as the daemon does, and clear the round's archives.
        for entry in fs::read_dir(&dest)? {
            fs::remove_file(entry?.path())?;
        }
    }

    // The test only means something if kills really landed mid-write and
    // backups also completed between kills.
    assert!(
        kills_with_partial > 0,
        "no kill ever interrupted a backup; make the world bigger"
    );
    assert!(
        finals_seen > 0,
        "no backup ever completed; lengthen the delay"
    );
    eprintln!(
        "one-backup={one_backup:?} rounds={ROUNDS} interrupted={kills_with_partial} complete-archives-checked={finals_seen}"
    );
    Ok(())
}
