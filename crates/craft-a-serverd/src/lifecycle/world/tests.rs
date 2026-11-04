// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! World imports against the fake runtime: no container engine.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use craft_a_server_core::config::Config;
use craft_a_server_core::mc_protocol::ServerStatus;
use craft_a_server_core::model::ServerState;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use super::*;
use crate::db::{Db, ModChange, ModRow};
use crate::lifecycle::{DiskUsage, Env};
use crate::runtime::{FakeRuntime, RuntimeCall};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

const GIB: u64 = 1024 * 1024 * 1024;
const LEVEL_DAT: &[u8] = &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1, 2, 3];

/// A server that comes up unless the world holds a `BROKEN` marker file.
struct FakeProbe {
    rt: FakeRuntime,
    watch: Mutex<Option<(String, PathBuf)>>,
}

impl StatusProbe for FakeProbe {
    async fn status(&self, _port: u16) -> Option<ServerStatus> {
        let watch = self
            .watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some((id, data)) = watch
            && data.join("world/BROKEN").exists()
        {
            self.rt.push_log(&id, "Failed to load the world");
            self.rt.simulate_exit(&id, 1, false);
            return None;
        }
        Some(ServerStatus {
            version_name: "26.3".into(),
            protocol: 777,
            players_online: 0,
            players_max: 20,
        })
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    svc: Service<FakeRuntime, FakeProbe>,
    rt: FakeRuntime,
    probe: Arc<FakeProbe>,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = Config {
        data_dir: dir.path().to_path_buf(),
        start_timeout_secs: 1,
        stop_timeout_secs: 5,
        ..Config::default()
    };
    let rt = FakeRuntime::new();
    let probe = Arc::new(FakeProbe {
        rt: rt.clone(),
        watch: Mutex::new(None),
    });
    let clock = Arc::new(AtomicI64::new(1_700_000_000));
    let env = Env {
        total_ram_bytes: 8 * GIB,
        uid: 1000,
        gid: 1000,
        now_unix: Arc::new(move || clock.fetch_add(100, Ordering::SeqCst)),
        disk: Arc::new(|_| {
            Ok(DiskUsage {
                free_bytes: 90 * GIB,
                total_bytes: 100 * GIB,
            })
        }),
        poll_interval: Duration::from_millis(10),
    };
    let svc = Service::new(
        Arc::new(cfg),
        Db::open_in_memory().expect("db"),
        Arc::new(rt.clone()),
        Arc::clone(&probe),
        env,
    );
    Harness {
        _dir: dir,
        svc,
        rt,
        probe,
    }
}

/// A server with a world, a mod folder and the files the container keeps.
async fn server(h: &Harness, run: bool) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let row = h.svc.create_server("Survival", "26.3", true).await?;
    let data = h.svc.data_dir(&row.id);
    *h.probe.watch.lock().unwrap_or_else(PoisonError::into_inner) =
        Some((row.id.clone(), data.clone()));
    fs::create_dir_all(data.join("world/region"))?;
    fs::write(data.join("world/level.dat"), LEVEL_DAT)?;
    fs::write(data.join("world/region/r.0.0.mca"), "old region")?;
    fs::create_dir_all(data.join("mods"))?;
    fs::write(data.join("mods/old.jar"), "old")?;
    fs::write(data.join("eula.txt"), "eula=true\n")?;
    fs::write(
        data.join("server.properties"),
        "online-mode=true\ndifficulty=easy\n",
    )?;
    if run {
        h.svc.start(&row.id).await?;
    }
    Ok(row.id)
}

fn upload(h: &Harness, id: &str, files: &[(&str, &[u8])]) -> TestResult {
    let mut writer = zip::ZipWriter::new(fs::File::create(h.svc.world_upload_path(id))?);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, data) in files {
        writer.start_file(*name, options)?;
        writer.write_all(data)?;
    }
    writer.finish()?;
    Ok(())
}

fn fabric_jar() -> std::result::Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file("fabric.mod.json", options)?;
    writer.write_all(b"{}")?;
    Ok(writer.finish()?.into_inner())
}

fn read(path: PathBuf) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn state(h: &Harness, id: &str) -> ServerState {
    h.svc.get_server(id).expect("server").state
}

fn starts(h: &Harness) -> usize {
    h.rt.calls()
        .iter()
        .filter(|c| matches!(c, RuntimeCall::CreateAndStart { .. }))
        .count()
}

fn leftovers(h: &Harness, id: &str) -> Vec<&'static str> {
    let dir = h.svc.server_dir(id);
    [
        "import.tmp",
        "data.old",
        "import.failed",
        "world-upload.zip",
    ]
    .into_iter()
    .filter(|n| dir.join(n).exists())
    .collect()
}

fn recorded_mod(id: &str) -> ModChange {
    ModChange {
        row: ModRow {
            server_id: id.to_owned(),
            project_id: "AAAAAAAA".into(),
            version_id: "v1".into(),
            title: "Old Mod".into(),
            version_number: "1".into(),
            file_name: "old.jar".into(),
            sha512: "0".repeat(128),
            size_bytes: 3,
            as_dependency: false,
            installed_unix: 1,
        },
        requires: Vec::new(),
    }
}

#[tokio::test]
async fn import_replaces_the_world_and_mods_restarts_and_checks() -> TestResult {
    let h = harness();
    let id = server(&h, true).await?;
    h.svc
        .db()
        .apply_mod_changes(&id, &[], &[recorded_mod(&id)])?;
    let jar = fabric_jar()?;
    upload(
        &h,
        &id,
        &[
            ("srv/world/level.dat", LEVEL_DAT),
            ("srv/world/region/r.0.0.mca", b"new region"),
            ("srv/mods/new.jar", &jar),
            (
                "srv/server.properties",
                b"difficulty=hard\nonline-mode=false\n",
            ),
        ],
    )?;
    let before = starts(&h);

    let outcome = h.svc.import_world(&id).await?;

    assert!(outcome.verified);
    assert_eq!(outcome.report.mods_imported, vec!["new.jar".to_owned()]);
    let data = h.svc.data_dir(&id);
    assert_eq!(read(data.join("world/region/r.0.0.mca")), "new region");
    assert!(data.join("mods/new.jar").is_file());
    assert!(!data.join("mods/old.jar").exists());
    assert_eq!(read(data.join("eula.txt")), "eula=true\n");
    let props = read(data.join("server.properties"));
    assert!(
        props.contains("difficulty=hard") && props.contains("online-mode=true"),
        "{props}"
    );
    assert_eq!(state(&h, &id), ServerState::Running);
    assert_eq!(starts(&h), before + 1);
    assert!(
        h.svc.db().list_mods(&id)?.is_empty(),
        "recorded mods are stale"
    );
    let kinds: Vec<String> = h
        .svc
        .list_backups(&id)?
        .into_iter()
        .map(|b| b.kind)
        .collect();
    assert!(kinds.contains(&"pre_change".to_owned()), "{kinds:?}");
    assert_eq!(leftovers(&h, &id), Vec::<&str>::new());
    Ok(())
}

#[tokio::test]
async fn a_world_that_stops_the_server_from_starting_is_rolled_back() -> TestResult {
    let h = harness();
    let id = server(&h, true).await?;
    h.svc
        .db()
        .apply_mod_changes(&id, &[], &[recorded_mod(&id)])?;
    upload(
        &h,
        &id,
        &[
            ("world/level.dat", LEVEL_DAT),
            ("world/BROKEN", b"x"),
            ("world/region/r.0.0.mca", b"new region"),
            ("mods/new.jar", &fabric_jar()?),
        ],
    )?;

    let err = h.svc.import_world(&id).await.expect_err("must fail");

    let ServiceError::WorldImport(text) = err else {
        panic!("expected a world import error, got {err:?}");
    };
    assert!(text.contains("previous setup was put back"), "{text}");
    let data = h.svc.data_dir(&id);
    assert_eq!(read(data.join("world/region/r.0.0.mca")), "old region");
    assert!(!data.join("world/BROKEN").exists());
    assert!(data.join("mods/old.jar").is_file() && !data.join("mods/new.jar").exists());
    assert_eq!(
        state(&h, &id),
        ServerState::Running,
        "the old setup runs again"
    );
    assert_eq!(
        h.svc.db().list_mods(&id)?.len(),
        1,
        "records are kept on a rollback"
    );
    assert_eq!(leftovers(&h, &id), Vec::<&str>::new());
    Ok(())
}

#[tokio::test]
async fn a_stopped_server_stays_stopped_and_is_not_verified() -> TestResult {
    let h = harness();
    let id = server(&h, false).await?;
    upload(
        &h,
        &id,
        &[
            ("world/level.dat", LEVEL_DAT),
            ("world/region/r.0.0.mca", b"new region"),
        ],
    )?;
    let before = starts(&h);

    let outcome = h.svc.import_world(&id).await?;

    assert!(!outcome.verified);
    assert_eq!(starts(&h), before);
    assert_eq!(state(&h, &id), ServerState::Stopped);
    assert_eq!(
        read(h.svc.data_dir(&id).join("world/region/r.0.0.mca")),
        "new region"
    );
    assert_eq!(
        read(h.svc.data_dir(&id).join("mods/old.jar")),
        "old",
        "a world-only zip keeps the mods"
    );
    Ok(())
}

#[tokio::test]
async fn a_bad_zip_changes_nothing_and_does_not_stop_the_server() -> TestResult {
    let h = harness();
    let id = server(&h, true).await?;
    fs::write(h.svc.world_upload_path(&id), b"not a zip")?;
    let calls = h.rt.calls().len();

    let err = h.svc.import_world(&id).await.expect_err("must fail");

    assert!(matches!(err, ServiceError::WorldImport(_)), "{err:?}");
    assert_eq!(h.rt.calls().len(), calls, "the container was not touched");
    assert_eq!(state(&h, &id), ServerState::Running);
    assert_eq!(
        read(h.svc.data_dir(&id).join("world/region/r.0.0.mca")),
        "old region"
    );
    assert_eq!(leftovers(&h, &id), Vec::<&str>::new());
    Ok(())
}

#[tokio::test]
async fn an_unsafe_zip_is_refused() -> TestResult {
    let h = harness();
    let id = server(&h, false).await?;
    upload(
        &h,
        &id,
        &[("world/level.dat", LEVEL_DAT), ("../../escape", b"x")],
    )?;
    let err = h.svc.import_world(&id).await.expect_err("must fail");
    assert!(matches!(err, ServiceError::WorldImport(_)), "{err:?}");
    assert!(!h._dir.path().join("escape").exists());
    Ok(())
}

#[tokio::test]
async fn too_little_disk_is_reported_before_anything_changes() -> TestResult {
    let mut h = harness();
    h.svc.env.disk = Arc::new(|_| {
        Ok(DiskUsage {
            free_bytes: 90 * GIB / 100,
            total_bytes: 100 * GIB,
        })
    });
    let id = server(&h, false).await?;
    upload(
        &h,
        &id,
        &[
            ("world/level.dat", LEVEL_DAT),
            ("world/big.bin", &vec![1_u8; 1000]),
        ],
    )?;
    // 0.9 GiB free: more than the 1% minimum? No: 0.9% < 15%, so the service refuses.
    let err = h.svc.import_world(&id).await.expect_err("must fail");
    assert!(matches!(err, ServiceError::DiskLow), "{err:?}");
    Ok(())
}

#[tokio::test]
async fn the_upload_budget_leaves_room_for_the_backup() -> TestResult {
    let h = harness();
    let id = server(&h, false).await?;
    let budget = h.svc.world_upload_budget(&id)?;
    assert!(budget > 0 && budget <= 90 * GIB / 3 + 1, "{budget}");
    Ok(())
}
