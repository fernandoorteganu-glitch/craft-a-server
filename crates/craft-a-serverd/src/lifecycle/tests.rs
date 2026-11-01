// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use craft_a_server_core::config::Config;
use craft_a_server_core::mc_protocol::ServerStatus;
use craft_a_server_core::model::ServerState;

use super::backups::Reason;
use super::server::Console;
use super::*;
use crate::runtime::{FakeRuntime, RuntimeCall, RuntimeOp};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

const GIB: u64 = 1024 * 1024 * 1024;

/// Probe whose answer is decided by a closure of "how many times was I asked".
struct FakeProbe {
    calls: AtomicUsize,
    answer: Box<dyn Fn(usize) -> bool + Send + Sync>,
}

impl FakeProbe {
    fn new(answer: impl Fn(usize) -> bool + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FakeProbe {
            calls: AtomicUsize::new(0),
            answer: Box::new(answer),
        })
    }

    fn always_up() -> Arc<Self> {
        Self::new(|_| true)
    }
}

impl StatusProbe for FakeProbe {
    async fn status(&self, _port: u16) -> Option<ServerStatus> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        (self.answer)(n).then(|| ServerStatus {
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
}

fn harness_with(probe: Arc<FakeProbe>, tweak: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config {
        data_dir: dir.path().to_path_buf(),
        start_timeout_secs: 1,
        stop_timeout_secs: 5,
        ..Config::default()
    };
    tweak(&mut cfg);
    let rt = FakeRuntime::new();
    let env = Env {
        total_ram_bytes: 8 * GIB,
        uid: 1000,
        gid: 1000,
        now_unix: Arc::new(|| 1_700_000_000),
        disk: Arc::new(|_| {
            Ok(DiskUsage {
                free_bytes: 90,
                total_bytes: 100,
            })
        }),
        poll_interval: Duration::from_millis(10),
    };
    let svc = Service::new(
        Arc::new(cfg),
        Db::open_in_memory().expect("db"),
        Arc::new(rt.clone()),
        probe,
        env,
    );
    Harness { _dir: dir, svc, rt }
}

fn harness() -> Harness {
    harness_with(FakeProbe::always_up(), |_| {})
}

async fn running_server(h: &Harness) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let row = h.svc.create_server("Survival", "26.3", true).await?;
    h.svc.start(&row.id).await?;
    Ok(row.id)
}

fn write(path: &Path, text: &str) -> TestResult {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

fn commands(rt: &FakeRuntime) -> Vec<String> {
    rt.calls()
        .into_iter()
        .filter_map(|c| match c {
            RuntimeCall::Rcon { command, .. } => Some(command),
            _ => None,
        })
        .collect()
}

fn state_of(h: &Harness, id: &str) -> std::result::Result<ServerState, Box<dyn std::error::Error>> {
    Ok(h.svc.get_server(id)?.state)
}

// ---- create ---------------------------------------------------------------

#[tokio::test]
async fn create_requires_the_eula() {
    let h = harness();
    assert!(matches!(
        h.svc.create_server("A", "26.3", false).await,
        Err(ServiceError::EulaNotAccepted)
    ));
    assert!(h.svc.list_servers().expect("list").is_empty());
}

#[tokio::test]
async fn create_validates_input() {
    let h = harness();
    for (name, version) in [
        ("", "26.3"),
        ("ok", ""),
        ("ok", "../x"),
        ("bad\nname", "26.3"),
    ] {
        assert!(
            matches!(
                h.svc.create_server(name, version, true).await,
                Err(ServiceError::InvalidInput(_))
            ),
            "{name:?} {version:?}"
        );
    }
}

#[tokio::test]
async fn create_makes_a_stopped_server_with_its_own_port() -> TestResult {
    let h = harness();
    let a = h.svc.create_server("One", "26.3", true).await?;
    let b = h.svc.create_server("Two", "26.3", true).await?;
    assert_eq!(a.state, ServerState::Stopped);
    assert_eq!(state_of(&h, &a.id)?, ServerState::Stopped);
    assert_ne!(a.host_port, b.host_port);
    assert!(h.svc.data_dir(&a.id).is_dir());
    assert!(a.rcon_password.len() >= 16);
    Ok(())
}

// ---- start / stop ---------------------------------------------------------

#[tokio::test]
async fn start_brings_the_server_to_running() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    assert_eq!(state_of(&h, &id)?, ServerState::Running);
    assert!(
        h.rt.calls()
            .contains(&RuntimeCall::CreateAndStart { id: id.clone() })
    );
    Ok(())
}

#[tokio::test]
async fn start_failure_marks_failed_and_cleans_up() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    h.rt.fail_next(
        RuntimeOp::CreateAndStart,
        crate::runtime::RuntimeError::Egress("nft failed".into()),
    );
    let err = h.svc.start(&row.id).await;
    assert!(matches!(err, Err(ServiceError::StartFailed(_))));
    assert!(matches!(state_of(&h, &row.id)?, ServerState::Failed(r) if r.contains("nft")));
    assert!(h.rt.pod_ids().is_empty());
    Ok(())
}

#[tokio::test]
async fn container_dying_during_startup_fails_fast_with_its_log() -> TestResult {
    let rt_slot: Arc<std::sync::Mutex<Option<(FakeRuntime, String)>>> = Arc::default();
    let slot = rt_slot.clone();
    let probe = FakeProbe::new(move |n| {
        if n == 2
            && let Some((rt, id)) = slot.lock().expect("lock").as_ref()
        {
            rt.push_log(id, "Exception in thread main: boom");
            rt.simulate_exit(id, 1, false);
        }
        false
    });
    let h = harness_with(probe, |c| c.start_timeout_secs = 30);
    let row = h.svc.create_server("A", "26.3", true).await?;
    *rt_slot.lock().expect("lock") = Some((h.rt.clone(), row.id.clone()));

    let started = std::time::Instant::now();
    let err = h.svc.start(&row.id).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not wait for the timeout"
    );
    match err {
        Err(ServiceError::StartFailed(reason)) => {
            assert!(reason.contains("code 1"), "{reason}");
            assert!(reason.contains("boom"), "{reason}");
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(matches!(state_of(&h, &row.id)?, ServerState::Failed(_)));
    Ok(())
}

#[tokio::test]
async fn slow_start_beyond_the_deadline_fails() -> TestResult {
    let h = harness_with(FakeProbe::new(|_| false), |c| c.start_timeout_secs = 1);
    let row = h.svc.create_server("A", "26.3", true).await?;
    let err = h.svc.start(&row.id).await;
    assert!(matches!(err, Err(ServiceError::StartFailed(r)) if r.contains("did not answer")));
    assert!(h.rt.pod_ids().is_empty(), "pod must be removed");
    Ok(())
}

#[tokio::test]
async fn slow_but_alive_start_succeeds_before_the_deadline() -> TestResult {
    let h = harness_with(FakeProbe::new(|n| n >= 5), |c| c.start_timeout_secs = 30);
    let id = running_server(&h).await?;
    assert_eq!(state_of(&h, &id)?, ServerState::Running);
    Ok(())
}

#[tokio::test]
async fn cannot_start_twice() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    assert!(matches!(
        h.svc.start(&id).await,
        Err(ServiceError::WrongState { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn stop_saves_then_stops_then_removes() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    h.svc.stop(&id).await?;
    assert_eq!(state_of(&h, &id)?, ServerState::Stopped);
    assert_eq!(commands(&h.rt), ["save-all flush", "stop"]);
    assert!(h.rt.pod_ids().is_empty());
    // Idempotent.
    h.svc.stop(&id).await?;
    Ok(())
}

#[tokio::test]
async fn stop_kills_when_the_graceful_stop_fails() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    h.rt.fail_next(
        RuntimeOp::Stop,
        crate::runtime::RuntimeError::Timeout {
            op: "stop".into(),
            secs: 1,
        },
    );
    h.svc.stop(&id).await?;
    assert!(h.rt.calls().contains(&RuntimeCall::Kill { id: id.clone() }));
    assert_eq!(state_of(&h, &id)?, ServerState::Stopped);
    Ok(())
}

#[tokio::test]
async fn failed_server_needs_a_reset_before_starting() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    h.rt.fail_next(
        RuntimeOp::CreateAndStart,
        crate::runtime::RuntimeError::Egress("x".into()),
    );
    let _ = h.svc.start(&row.id).await;
    assert!(matches!(
        h.svc.start(&row.id).await,
        Err(ServiceError::WrongState { .. })
    ));
    h.svc.reset(&row.id).await?;
    h.svc.start(&row.id).await?;
    assert_eq!(state_of(&h, &row.id)?, ServerState::Running);
    Ok(())
}

#[tokio::test]
async fn not_enough_memory_refuses_to_start() -> TestResult {
    let mut h = harness();
    h.svc.env.total_ram_bytes = 2 * GIB;
    let row = h.svc.create_server("A", "26.3", true).await?;
    assert!(matches!(
        h.svc.start(&row.id).await,
        Err(ServiceError::Memory(_))
    ));
    assert_eq!(state_of(&h, &row.id)?, ServerState::Stopped);
    Ok(())
}

#[tokio::test]
async fn console_only_accepts_valid_player_names() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    h.svc
        .console(&id, &Console::WhitelistAdd("Steve_01".into()))
        .await?;
    assert!(commands(&h.rt).contains(&"whitelist add Steve_01".to_owned()));
    for bad in ["", "ab", "a b", "x; stop", "name\n", "waytoolongname_12345"] {
        assert!(
            h.svc
                .console(&id, &Console::Kick(bad.into()))
                .await
                .is_err(),
            "{bad:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn delete_removes_data_and_backups() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    write(&h.svc.data_dir(&row.id).join("world/level.dat"), "x")?;
    h.svc.backup(&row.id, Reason::Manual).await?;
    h.svc.delete_server(&row.id).await?;
    assert!(h.svc.list_servers()?.is_empty());
    assert!(!h.svc.server_dir(&row.id).exists());
    assert!(!h.svc.server_backups_dir(&row.id).exists());
    Ok(())
}

// ---- backups --------------------------------------------------------------

fn seed_world(h: &Harness, id: &str, text: &str) -> TestResult {
    let data = h.svc.data_dir(id);
    write(&data.join("world/level.dat"), text)?;
    write(&data.join("world/region/r.0.0.mca"), &text.repeat(100))?;
    write(&data.join("logs/latest.log"), "noise")?;
    Ok(())
}

#[tokio::test]
async fn backup_of_a_running_server_pauses_and_resumes_saving() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    seed_world(&h, &id, "v1")?;
    let b = h.svc.backup(&id, Reason::Manual).await?;
    assert!(b.verified);
    assert!(h.svc.backup_path(&id, &b.file_name).is_file());
    let cmds = commands(&h.rt);
    assert_eq!(cmds, ["save-off", "save-all flush", "save-on"]);
    assert_eq!(h.svc.list_backups(&id)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn saving_is_always_re_enabled_even_if_the_copy_fails() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    // No data directory: the archive step fails after save-off.
    fs::remove_dir_all(h.svc.data_dir(&id))?;
    assert!(h.svc.backup(&id, Reason::Manual).await.is_err());
    assert_eq!(commands(&h.rt).last().map(String::as_str), Some("save-on"));
    assert!(h.svc.list_backups(&id)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn backup_of_a_stopped_server_skips_rcon() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    seed_world(&h, &row.id, "v1")?;
    h.svc.backup(&row.id, Reason::Manual).await?;
    assert!(commands(&h.rt).is_empty());
    Ok(())
}

#[tokio::test]
async fn low_disk_blocks_backups_before_touching_the_server() -> TestResult {
    let mut h = harness();
    h.svc.env.disk = Arc::new(|_| {
        Ok(DiskUsage {
            free_bytes: 5,
            total_bytes: 100,
        })
    });
    let id = running_server(&h).await?;
    seed_world(&h, &id, "v1")?;
    assert!(matches!(
        h.svc.backup(&id, Reason::Manual).await,
        Err(ServiceError::DiskLow)
    ));
    assert!(commands(&h.rt).is_empty(), "server must not be paused");
    Ok(())
}

#[tokio::test]
async fn restore_brings_back_the_old_world_and_keeps_a_safety_backup() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    seed_world(&h, &id, "v1")?;
    let backup = h.svc.backup(&id, Reason::Manual).await?;

    write(&h.svc.data_dir(&id).join("world/level.dat"), "v2-newer")?;
    // Different timestamp so the pre-restore name cannot collide.
    let mut h = h;
    h.svc.env.now_unix = Arc::new(|| 1_700_000_100);

    h.svc.restore(&id, &backup.id).await?;

    let level = fs::read_to_string(h.svc.data_dir(&id).join("world/level.dat"))?;
    assert_eq!(level, "v1");
    assert_eq!(
        state_of(&h, &id)?,
        ServerState::Running,
        "restarts what was running"
    );
    let kinds: Vec<_> = h
        .svc
        .list_backups(&id)?
        .into_iter()
        .map(|b| b.kind)
        .collect();
    assert!(kinds.contains(&"pre_restore".to_owned()), "{kinds:?}");
    assert!(!h.svc.server_dir(&id).join("data.old").exists());
    assert!(!h.svc.server_dir(&id).join("restore.tmp").exists());
    Ok(())
}

#[tokio::test]
async fn restore_of_a_corrupt_backup_changes_nothing() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    seed_world(&h, &row.id, "v1")?;
    let backup = h.svc.backup(&row.id, Reason::Manual).await?;
    let file = h.svc.backup_path(&row.id, &backup.file_name);
    let mut bytes = fs::read(&file)?;
    let mid = bytes.len() / 2;
    if let Some(b) = bytes.get_mut(mid) {
        *b ^= 0xff;
    }
    fs::write(&file, bytes)?;

    write(&h.svc.data_dir(&row.id).join("world/level.dat"), "current")?;
    assert!(h.svc.restore(&row.id, &backup.id).await.is_err());
    let level = fs::read_to_string(h.svc.data_dir(&row.id).join("world/level.dat"))?;
    assert_eq!(level, "current");
    Ok(())
}

#[tokio::test]
async fn restore_rejects_a_backup_of_another_server() -> TestResult {
    let h = harness();
    let a = h.svc.create_server("A", "26.3", true).await?;
    let b = h.svc.create_server("B", "26.3", true).await?;
    seed_world(&h, &a.id, "v1")?;
    let backup = h.svc.backup(&a.id, Reason::Manual).await?;
    assert!(matches!(
        h.svc.restore(&b.id, &backup.id).await,
        Err(ServiceError::NotFound(_))
    ));
    Ok(())
}

#[tokio::test]
async fn scheduled_backup_runs_once_per_slot() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    // Nothing to save before the first world exists.
    assert!(h.svc.run_due_backups().await.is_empty());
    seed_world(&h, &row.id, "v1")?;

    let first = h.svc.run_due_backups().await;
    assert_eq!(first.len(), 1);
    assert!(first.iter().all(|(_, r)| r.is_ok()));
    assert!(
        h.svc.run_due_backups().await.is_empty(),
        "already done for this slot"
    );
    Ok(())
}

#[tokio::test]
async fn retention_prunes_old_dailies_but_keeps_manual() -> TestResult {
    let mut h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    seed_world(&h, &row.id, "v1")?;
    let base = 1_700_000_000_i64;
    let manual = {
        h.svc.env.now_unix = Arc::new(move || base);
        h.svc.backup(&row.id, Reason::Manual).await?
    };
    // 40 daily backups on consecutive days.
    for day in 1..=40_i64 {
        let t = base + day * 86_400;
        h.svc.env.now_unix = Arc::new(move || t);
        h.svc.backup(&row.id, Reason::Daily).await?;
    }
    let left = h.svc.list_backups(&row.id)?;
    assert!(
        left.len() < 41,
        "retention must prune ({} left)",
        left.len()
    );
    assert!(
        left.iter().any(|b| b.id == manual.id),
        "manual backups are kept"
    );
    assert!(
        left.iter()
            .all(|b| h.svc.backup_path(&row.id, &b.file_name).is_file()),
        "every remaining record has its file"
    );
    let files = fs::read_dir(h.svc.server_backups_dir(&row.id))?.count();
    assert_eq!(files, left.len(), "no orphaned archives");
    Ok(())
}

// ---- recovery -------------------------------------------------------------

#[tokio::test]
async fn reconcile_marks_vanished_unwanted_servers_failed() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    h.svc.db().set_auto_start(&id, false)?; // not a server we promised to bring back
    h.rt.simulate_vanish(&id);
    let report = h.svc.reconcile().await?;
    assert_eq!(report.state_corrected, std::slice::from_ref(&id));
    assert!(matches!(state_of(&h, &id)?, ServerState::Failed(_)));
    Ok(())
}

#[tokio::test]
async fn reconcile_re_enables_autosave_on_running_servers() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    let report = h.svc.reconcile().await?;
    assert_eq!(report.autosave_restored, std::slice::from_ref(&id));
    assert_eq!(commands(&h.rt).last().map(String::as_str), Some("save-on"));
    assert_eq!(state_of(&h, &id)?, ServerState::Running);
    Ok(())
}

#[tokio::test]
async fn reconcile_recovers_an_interrupted_swap() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    // Crash between "live -> trash" and "restored -> live".
    let live = h.svc.data_dir(&row.id);
    let trash = h.svc.server_dir(&row.id).join("data.old");
    write(&live.join("world/level.dat"), "precious")?;
    fs::rename(&live, &trash)?;
    assert!(!live.exists());

    let report = h.svc.reconcile().await?;
    assert_eq!(report.swaps_recovered, std::slice::from_ref(&row.id));
    assert_eq!(
        fs::read_to_string(live.join("world/level.dat"))?,
        "precious"
    );
    Ok(())
}

#[tokio::test]
async fn reconcile_removes_partial_backups_but_keeps_real_ones() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    seed_world(&h, &row.id, "v1")?;
    let good = h.svc.backup(&row.id, Reason::Manual).await?;
    let dir = h.svc.server_backups_dir(&row.id);
    write(&dir.join("x-1-daily.tar.zst.partial"), "half")?;

    let report = h.svc.reconcile().await?;
    assert_eq!(report.partial_backups_removed, 1);
    assert!(h.svc.backup_path(&row.id, &good.file_name).is_file());
    assert!(!dir.join("x-1-daily.tar.zst.partial").exists());
    Ok(())
}

#[tokio::test]
async fn reconcile_reports_orphan_pods_without_touching_them() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    h.svc.db().delete_server(&id)?;
    let report = h.svc.reconcile().await?;
    assert_eq!(report.orphan_pods.len(), 1);
    assert_eq!(h.rt.pod_ids(), [id]);
    Ok(())
}

#[tokio::test]
async fn crashed_server_is_restarted_until_the_limit() -> TestResult {
    let h = harness_with(FakeProbe::always_up(), |c| c.max_consecutive_failures = 2);
    let id = running_server(&h).await?;

    h.rt.simulate_exit(&id, 137, true);
    h.svc.supervise_once().await?;
    assert_eq!(
        state_of(&h, &id)?,
        ServerState::Running,
        "first crash: restart"
    );

    h.rt.simulate_exit(&id, 1, false);
    h.svc.supervise_once().await?;
    assert!(
        matches!(state_of(&h, &id)?, ServerState::Failed(ref r) if r.contains("crashed")),
        "second crash hits the limit: stays failed, got {:?}",
        state_of(&h, &id)?
    );
    let before = h.rt.calls().len();
    h.svc.supervise_once().await?;
    assert_eq!(h.rt.calls().len(), before, "no more retries once failed");
    Ok(())
}

#[tokio::test]
async fn healthy_servers_are_left_alone_by_the_supervisor() -> TestResult {
    let h = harness();
    running_server(&h).await?;
    let starts = || {
        h.rt.calls()
            .iter()
            .filter(|c| matches!(c, RuntimeCall::CreateAndStart { .. }))
            .count()
    };
    let before = starts();
    h.svc.supervise_once().await?;
    assert_eq!(starts(), before);
    Ok(())
}

// ---- auto-start -----------------------------------------------------------

#[tokio::test]
async fn running_servers_are_marked_to_come_back_and_stopped_ones_are_not() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    assert!(h.svc.get_server(&id)?.auto_start);
    h.svc.stop(&id).await?;
    assert!(
        !h.svc.get_server(&id)?.auto_start,
        "an explicit stop is respected"
    );
    Ok(())
}

#[tokio::test]
async fn a_server_that_cannot_start_does_not_retry_at_boot() -> TestResult {
    let h = harness();
    let row = h.svc.create_server("A", "26.3", true).await?;
    h.rt.fail_next(
        RuntimeOp::CreateAndStart,
        crate::runtime::RuntimeError::Egress("x".into()),
    );
    let _ = h.svc.start(&row.id).await;
    assert!(!h.svc.get_server(&row.id)?.auto_start);
    Ok(())
}

#[tokio::test]
async fn after_a_power_cut_a_wanted_server_is_queued_not_failed() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    h.rt.simulate_vanish(&id); // the machine lost power: no container any more

    let report = h.svc.reconcile().await?;
    assert_eq!(report.to_autostart, std::slice::from_ref(&id));
    assert!(report.state_corrected.is_empty());
    assert_eq!(state_of(&h, &id)?, ServerState::Stopped);

    h.svc.autostart(&report.to_autostart).await;
    assert_eq!(state_of(&h, &id)?, ServerState::Running);
    Ok(())
}

#[tokio::test]
async fn a_server_the_user_stopped_stays_stopped_after_reboot() -> TestResult {
    let h = harness();
    let id = running_server(&h).await?;
    h.svc.stop(&id).await?;
    let report = h.svc.reconcile().await?;
    assert!(report.to_autostart.is_empty());
    assert_eq!(state_of(&h, &id)?, ServerState::Stopped);
    Ok(())
}

#[tokio::test]
async fn giving_up_on_a_crash_loop_turns_auto_start_off() -> TestResult {
    let h = harness_with(FakeProbe::always_up(), |c| c.max_consecutive_failures = 1);
    let id = running_server(&h).await?;
    h.rt.simulate_exit(&id, 1, false);
    h.svc.supervise_once().await?;
    assert!(matches!(state_of(&h, &id)?, ServerState::Failed(_)));
    assert!(!h.svc.get_server(&id)?.auto_start);
    Ok(())
}

#[tokio::test]
async fn the_image_tag_follows_the_minecraft_version() -> TestResult {
    let h = harness();
    let old = h.svc.create_server("Old", "1.20.1", true).await?;
    let new = h.svc.create_server("New", "26.3", true).await?;
    let plan = craft_a_server_core::memory::plan(8 * GIB, 1536, None)?;
    let image_of =
        |id: &str| -> Result<String> { Ok(h.svc.build_spec(&h.svc.get_row(id)?, &plan).image) };
    assert!(image_of(&old.id)?.ends_with(":java17"));
    assert!(image_of(&new.id)?.ends_with(":java25"));
    Ok(())
}
