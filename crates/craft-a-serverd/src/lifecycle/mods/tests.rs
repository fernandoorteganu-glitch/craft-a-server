// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! Mod transactions against the fake runtime and the fake Modrinth: no
//! network, no container engine.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use craft_a_server_core::config::Config;
use craft_a_server_core::mc_protocol::ServerStatus;
use craft_a_server_core::model::ServerState;
use serde_json::{Value, json};

use super::*;
use crate::db::Db;
use crate::lifecycle::{DiskUsage, Env};
use crate::mods::testutil::{FakeTransport, Reply, Synth, serve_synth};
use crate::runtime::{FakeRuntime, RuntimeCall};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

const GIB: u64 = 1024 * 1024 * 1024;
const ALPHA: &str = "AAAAAAAA"; // requires BETA
const BETA: &str = "BBBBBBBB";
const BAD: &str = "CCCCCCCC"; // ships bad.jar, which the fake server cannot start with
const CLIENT_ONLY: &str = "DDDDDDDD";
const TAMPERED: &str = "EEEEEEEE";
const FOE: &str = "FFFFFFFF"; // incompatible with BETA

/// A server that comes up unless `bad.jar` is in its mods folder (then the
/// "container" exits with a log line, like a real crash on start).
struct FakeProbe {
    rt: FakeRuntime,
    watch: Mutex<Option<(String, PathBuf)>>,
    asked: AtomicUsize,
}

impl StatusProbe for FakeProbe {
    async fn status(&self, _port: u16) -> Option<ServerStatus> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let watch = self
            .watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some((id, mods)) = watch
            && mods.join("bad.jar").exists()
        {
            self.rt.push_log(&id, "Mixin apply failed: bad.jar");
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
    net: FakeTransport,
    probe: Arc<FakeProbe>,
}

fn build(with_modrinth: bool) -> Harness {
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
        asked: AtomicUsize::new(0),
    });
    let clock = Arc::new(AtomicI64::new(1_700_000_000));
    let env = Env {
        total_ram_bytes: 8 * GIB,
        uid: 1000,
        gid: 1000,
        now_unix: Arc::new(move || clock.fetch_add(100, Ordering::SeqCst)),
        disk: Arc::new(|_| {
            Ok(DiskUsage {
                free_bytes: 90,
                total_bytes: 100,
            })
        }),
        poll_interval: Duration::from_millis(10),
    };
    let net = FakeTransport::new();
    let mut svc = Service::new(
        Arc::new(cfg),
        Db::open_in_memory().expect("db"),
        Arc::new(rt.clone()),
        Arc::clone(&probe),
        env,
    );
    if with_modrinth {
        svc = svc.with_modrinth(net.client());
    }
    serve_world(&net);
    Harness {
        _dir: dir,
        svc,
        rt,
        net,
        probe,
    }
}

fn harness() -> Harness {
    build(true)
}

fn serve_world(t: &FakeTransport) {
    serve_synth(t, "Beta Lib", "required", &[Synth::new(BETA, "b1")]);
    serve_synth(
        t,
        "Alpha Mod",
        "optional",
        &[Synth::new(ALPHA, "a1").dep(BETA, "required")],
    );
    let mut bad = Synth::new(BAD, "c1");
    bad.file_name = "bad.jar".into();
    serve_synth(t, "Bad Mod", "required", &[bad]);
    serve_synth(
        t,
        "Client Only",
        "unsupported",
        &[Synth::new(CLIENT_ONLY, "d1")],
    );
    serve_synth(t, "Tampered", "required", &[Synth::new(TAMPERED, "e1")]);
    // The CDN serves other bytes than the API announced.
    let e = Synth::new(TAMPERED, "e1");
    let mut other = e.jar.clone();
    if let Some(b) = other.get_mut(12) {
        *b ^= 0x55;
    }
    t.cdn(&e.url(), &other);
    serve_synth(
        t,
        "Foe",
        "required",
        &[Synth::new(FOE, "f1").dep(BETA, "incompatible")],
    );
    set_titles(t);
}

fn set_titles(t: &FakeTransport) {
    let p = |id: &str, title: &str| json!({"id": id, "title": title, "project_type": "mod", "server_side": "required"});
    t.api(
        "/projects",
        &Value::Array(vec![
            p(ALPHA, "Alpha Mod"),
            p(BETA, "Beta Lib"),
            p(BAD, "Bad Mod"),
            p(FOE, "Foe"),
        ]),
    );
}

async fn running(h: &Harness) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let row = h.svc.create_server("Survival", "26.3", true).await?;
    let mods = h.svc.mods_dir(&row.id);
    *h.probe.watch.lock().unwrap_or_else(PoisonError::into_inner) = Some((row.id.clone(), mods));
    h.svc.start(&row.id).await?;
    fs::write(h.svc.data_dir(&row.id).join("world.dat"), "world-v1")?;
    Ok(row.id)
}

async fn stopped(h: &Harness) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let row = h.svc.create_server("Survival", "26.3", true).await?;
    fs::write(h.svc.data_dir(&row.id).join("world.dat"), "world-v1")?;
    Ok(row.id)
}

fn jars(h: &Harness, id: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(h.svc.mods_dir(id))
        .map(|d| {
            d.filter_map(std::result::Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn starts(h: &Harness) -> usize {
    h.rt.calls()
        .iter()
        .filter(|c| matches!(c, RuntimeCall::CreateAndStart { .. }))
        .count()
}

fn stops(h: &Harness) -> usize {
    h.rt.calls()
        .iter()
        .filter(|c| matches!(c, RuntimeCall::Stop { .. }))
        .count()
}

fn state(h: &Harness, id: &str) -> ServerState {
    h.svc.get_server(id).expect("server").state
}

fn kinds(h: &Harness, id: &str) -> Vec<String> {
    h.svc
        .list_backups(id)
        .expect("backups")
        .into_iter()
        .map(|b| b.kind)
        .collect()
}

fn mods_error(e: ServiceError) -> ModError {
    match e {
        ServiceError::Mods(m) => m,
        other => panic!("expected a mods error, got {other:?}"),
    }
}

fn txn_gone(h: &Harness, id: &str) -> bool {
    !h.svc.txn_root(id).exists()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

// ---- install -----------------------------------------------------------------

#[tokio::test]
async fn install_snapshots_restarts_checks_and_records() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    let before = starts(&h);

    let change = h.svc.mods_install(&id, ALPHA, false).await?;

    assert!(change.verified);
    let names: Vec<&str> = change
        .installed
        .iter()
        .map(|m| m.project_id.as_str())
        .collect();
    assert_eq!(names, [BETA, ALPHA], "dependencies first");
    assert_eq!(
        jars(&h, &id),
        [format!("{ALPHA}-1.0.0.jar"), format!("{BETA}-1.0.0.jar")]
    );
    // Snapshot of the quiet server, then a restart.
    assert!(kinds(&h, &id).contains(&"pre_change".to_owned()));
    assert_eq!(stops(&h), 1);
    assert_eq!(starts(&h), before + 1);
    assert_eq!(state(&h, &id), ServerState::Running);
    // Recorded, the dependency flagged as such.
    let rows = h.svc.db().list_mods(&id)?;
    assert_eq!(rows.len(), 2);
    let beta = rows.iter().find(|r| r.project_id == BETA).expect("beta");
    let alpha = rows.iter().find(|r| r.project_id == ALPHA).expect("alpha");
    assert!(beta.as_dependency && !alpha.as_dependency);
    assert_eq!(alpha.sha512.len(), 128);
    assert!(txn_gone(&h, &id));
    // Dependency edges are known afterwards.
    assert_eq!(h.svc.db().mod_dependents(&id, BETA)?, [ALPHA]);
    Ok(())
}

#[tokio::test]
async fn the_snapshot_holds_the_old_mods_folder() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    fs::create_dir_all(h.svc.mods_dir(&id))?;
    fs::write(h.svc.mods_dir(&id).join("old.jar"), "old")?;
    h.svc.mods_install(&id, BETA, false).await?;
    let snap = h
        .svc
        .list_backups(&id)?
        .into_iter()
        .find(|b| b.kind == "pre_change")
        .expect("snapshot");
    assert!(snap.verified);
    assert!(h.svc.backup_path(&id, &snap.file_name).is_file());
    Ok(())
}

#[tokio::test]
async fn a_stopped_server_gets_the_files_but_no_health_check() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    let change = h.svc.mods_install(&id, ALPHA, false).await?;
    assert!(!change.verified);
    assert_eq!(starts(&h), 0);
    assert_eq!(state(&h, &id), ServerState::Stopped);
    assert_eq!(jars(&h, &id).len(), 2);
    assert_eq!(h.svc.db().list_mods(&id)?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_failed_health_check_restores_the_snapshot_and_says_why() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    fs::create_dir_all(h.svc.mods_dir(&id))?;
    fs::write(h.svc.mods_dir(&id).join("keep.jar"), "keep")?;

    let err = h
        .svc
        .mods_install(&id, BAD, false)
        .await
        .expect_err("must fail");
    match mods_error(err) {
        ModError::RolledBack { reason } => assert!(reason.contains("bad.jar"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // Everything as before: files, records, running server, world.
    assert_eq!(jars(&h, &id), ["keep.jar"]);
    assert_eq!(read(&h.svc.mods_dir(&id).join("keep.jar")), "keep");
    assert!(h.svc.db().list_mods(&id)?.is_empty());
    assert_eq!(state(&h, &id), ServerState::Running);
    assert_eq!(read(&h.svc.data_dir(&id).join("world.dat")), "world-v1");
    assert!(txn_gone(&h, &id));
    assert!(kinds(&h, &id).contains(&"pre_change".to_owned()));
    // The failed start plus the start of the old setup.
    assert!(starts(&h) >= 3);
    Ok(())
}

#[tokio::test]
async fn a_rollback_after_other_mods_were_installed_keeps_those() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    h.svc.mods_install(&id, BETA, false).await?;
    let err = h
        .svc
        .mods_install(&id, BAD, false)
        .await
        .expect_err("must fail");
    assert!(matches!(mods_error(err), ModError::RolledBack { .. }));
    assert_eq!(jars(&h, &id), [format!("{BETA}-1.0.0.jar")]);
    assert_eq!(h.svc.db().list_mods(&id)?.len(), 1);
    assert_eq!(state(&h, &id), ServerState::Running);
    Ok(())
}

#[tokio::test]
async fn a_download_that_fails_verification_never_touches_the_server() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    let calls = h.rt.calls().len();
    let err = h
        .svc
        .mods_install(&id, TAMPERED, false)
        .await
        .expect_err("fails");
    assert!(matches!(mods_error(err), ModError::HashMismatch { .. }));
    assert_eq!(h.rt.calls().len(), calls, "no stop, no restart");
    assert!(
        kinds(&h, &id).is_empty(),
        "no snapshot for a change that never started"
    );
    assert!(jars(&h, &id).is_empty());
    assert!(h.svc.db().list_mods(&id)?.is_empty());
    assert!(txn_gone(&h, &id));
    assert_eq!(state(&h, &id), ServerState::Running);
    Ok(())
}

#[tokio::test]
async fn an_offhost_or_redirected_download_is_refused_the_same_way() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    let a = Synth::new(BETA, "b1");
    h.net.route(&a.url(), vec![Reply::Redirect(302)]);
    let calls = h.rt.calls().len();
    let err = h
        .svc
        .mods_install(&id, BETA, false)
        .await
        .expect_err("fails");
    assert!(matches!(mods_error(err), ModError::Http(_)));
    assert_eq!(h.rt.calls().len(), calls);
    assert!(jars(&h, &id).is_empty());

    // A version that points at another host.
    let mut evil = Synth::new(FOE, "f9");
    evil.deps.clear();
    let mut v = evil.json();
    if let Some(url) = v.pointer_mut("/files/0/url") {
        *url = json!("https://evil.example/f.jar");
    }
    h.net.api(&format!("/project/{FOE}/version"), &json!([v]));
    let err = h
        .svc
        .mods_install(&id, FOE, false)
        .await
        .expect_err("fails");
    assert!(
        matches!(mods_error(err), ModError::ForbiddenHost(_)),
        "wrong host"
    );
    assert_eq!(h.rt.calls().len(), calls);
    Ok(())
}

#[tokio::test]
async fn client_only_mods_and_unknown_projects_are_refused_early() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    let err = h
        .svc
        .mods_install(&id, CLIENT_ONLY, false)
        .await
        .expect_err("fails");
    assert!(matches!(mods_error(err), ModError::ClientOnly(_)));
    let err = h
        .svc
        .mods_install(&id, "ZZZZZZZZ", false)
        .await
        .expect_err("fails");
    assert!(matches!(mods_error(err), ModError::ProjectNotFound(_)));
    let err = h
        .svc
        .mods_install(&id, "../x", false)
        .await
        .expect_err("fails");
    assert!(matches!(mods_error(err), ModError::InvalidInput(_)));
    assert_eq!(stops(&h), 0);
    Ok(())
}

#[tokio::test]
async fn incompatible_mods_are_refused_with_names() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    h.svc.mods_install(&id, BETA, false).await?;
    let plan = h.svc.mods_plan(&id, FOE, false).await?;
    assert_eq!(plan.conflicts.len(), 1);
    assert!(
        plan.conflicts.iter().any(|c| c.contains("Beta Lib")),
        "{:?}",
        plan.conflicts
    );
    let stops_before = stops(&h);
    let err = h
        .svc
        .mods_install(&id, FOE, false)
        .await
        .expect_err("fails");
    assert!(matches!(mods_error(err), ModError::Conflicts(_)));
    assert_eq!(stops(&h), stops_before);
    assert_eq!(jars(&h, &id).len(), 1);
    Ok(())
}

#[tokio::test]
async fn an_unmanaged_jar_with_the_same_name_is_never_overwritten() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    let name = format!("{BETA}-1.0.0.jar");
    fs::create_dir_all(h.svc.mods_dir(&id))?;
    fs::write(h.svc.mods_dir(&id).join(&name), "mine")?;
    let change = h.svc.mods_install(&id, BETA, false).await?;
    assert_eq!(read(&h.svc.mods_dir(&id).join(&name)), "mine");
    let placed = change.installed.first().expect("one").file_name.clone();
    assert_ne!(placed, name);
    assert!(h.svc.mods_dir(&id).join(placed).is_file());
    let list = h.svc.mods_list(&id).await?;
    assert_eq!(list.untracked, [name]);
    Ok(())
}

#[tokio::test]
async fn installing_the_same_version_twice_is_refused() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    h.svc.mods_install(&id, BETA, false).await?;
    let err = h
        .svc
        .mods_install(&id, BETA, false)
        .await
        .expect_err("fails");
    assert!(matches!(mods_error(err), ModError::AlreadyInstalled(_)));
    assert_eq!(h.svc.db().list_mods(&id)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_server_that_is_busy_or_failed_cannot_be_changed() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    h.svc
        .db()
        .set_state(&id, &ServerState::Failed("x".into()))?;
    let err = h
        .svc
        .mods_install(&id, BETA, false)
        .await
        .expect_err("fails");
    assert!(matches!(err, ServiceError::WrongState { .. }));
    assert!(jars(&h, &id).is_empty());
    assert!(txn_gone(&h, &id));
    Ok(())
}

#[tokio::test]
async fn without_a_client_mods_are_not_configured() -> TestResult {
    let h = build(false);
    let id = stopped(&h).await?;
    for e in [
        h.svc.mods_search(&id, "x", false, 0).await.expect_err("e"),
        h.svc.mods_install(&id, BETA, false).await.expect_err("e"),
        h.svc.mods_check(&id).await.expect_err("e"),
    ] {
        assert!(matches!(mods_error(e), ModError::NotConfigured));
    }
    // Listing needs no network.
    assert!(h.svc.mods_list(&id).await?.mods.is_empty());
    Ok(())
}

// ---- update ------------------------------------------------------------------

#[tokio::test]
async fn an_update_replaces_the_old_file_and_the_check_finds_it() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    h.svc.mods_install(&id, BETA, false).await?;
    let old = Synth::new(BETA, "b1");
    let mut new = Synth::new(BETA, "b2").number("2.0.0");
    new.file_name = format!("{BETA}-2.0.0.jar");
    new.date = "2026-10-20T00:00:00Z".into();
    new.jar = crate::mods::files::testutil::fabric_jar(b"second build");
    serve_synth(&h.net, "Beta Lib", "required", &[old.clone(), new.clone()]);
    set_titles(&h.net);

    h.net.api(
        "/version_files/update",
        &json!({ old.sha512(): new.json() }),
    );
    let updates = h.svc.mods_check(&id).await?;
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates.first().map(|u| u.latest_version.as_str()),
        Some("2.0.0")
    );

    let plan = h.svc.mods_plan(&id, BETA, false).await?;
    assert!(plan.replaces.is_some());
    h.svc.mods_install(&id, BETA, false).await?;
    assert_eq!(jars(&h, &id), [format!("{BETA}-2.0.0.jar")]);
    let rows = h.svc.db().list_mods(&id)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first().map(|r| r.version_number.as_str()),
        Some("2.0.0")
    );
    Ok(())
}

// ---- remove ------------------------------------------------------------------

#[tokio::test]
async fn removal_warns_about_dependents_and_needs_force() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    h.svc.mods_install(&id, ALPHA, false).await?;
    let plan = h.svc.mods_remove_plan(&id, BETA)?;
    assert_eq!(plan.dependents, ["Alpha Mod"]);

    let err = h
        .svc
        .mods_remove(&id, BETA, false, false)
        .await
        .expect_err("refused");
    assert!(matches!(mods_error(err), ModError::HasDependents { .. }));
    assert_eq!(jars(&h, &id).len(), 2);

    let change = h.svc.mods_remove(&id, BETA, true, false).await?;
    assert_eq!(change.removed, ["Beta Lib"]);
    assert_eq!(jars(&h, &id), [format!("{ALPHA}-1.0.0.jar")]);
    let rows = h.svc.db().list_mods(&id)?;
    assert_eq!(rows.len(), 1);
    assert!(kinds(&h, &id).contains(&"pre_change".to_owned()));
    Ok(())
}

#[tokio::test]
async fn removing_a_mod_can_take_its_automatic_dependencies_along() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    h.svc.mods_install(&id, ALPHA, false).await?;
    let plan = h.svc.mods_remove_plan(&id, ALPHA)?;
    assert_eq!(plan.orphans, ["Beta Lib"]);
    // By exact title as well as by id.
    h.svc.mods_remove(&id, "alpha mod", false, true).await?;
    assert!(jars(&h, &id).is_empty());
    assert!(h.svc.db().list_mods(&id)?.is_empty());
    let err = h
        .svc
        .mods_remove(&id, ALPHA, false, false)
        .await
        .expect_err("gone");
    assert!(matches!(mods_error(err), ModError::NotInstalled(_)));
    Ok(())
}

#[tokio::test]
async fn a_removal_that_breaks_the_server_is_rolled_back_too() -> TestResult {
    let h = harness();
    let id = running(&h).await?;
    h.svc.mods_install(&id, BETA, false).await?;
    // The server now refuses to start whenever bad.jar exists; plant one so
    // the restart after the removal fails its health check.
    fs::write(h.svc.mods_dir(&id).join("bad.jar"), "x")?;
    let err = h
        .svc
        .mods_remove(&id, BETA, false, false)
        .await
        .expect_err("fails");
    assert!(matches!(
        mods_error(err),
        ModError::RollbackFailed { .. } | ModError::RolledBack { .. }
    ));
    // Whatever the verdict, the removed jar is back in the folder and recorded.
    assert!(jars(&h, &id).contains(&format!("{BETA}-1.0.0.jar")));
    assert_eq!(h.svc.db().list_mods(&id)?.len(), 1);
    Ok(())
}

// ---- search / list -------------------------------------------------------------

#[tokio::test]
async fn search_results_are_cleaned_and_marked_when_installed() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    h.svc.mods_install(&id, BETA, false).await?;
    h.net.api(
        "/search",
        &json!({"hits": [
            {"project_id": BETA, "slug": "beta", "title": "Beta\u{7}Lib", "description": "d".repeat(900),
             "author": "me", "downloads": 5, "server_side": "required", "project_type": "mod"},
            {"project_id": ALPHA, "slug": "a", "title": "<script>alert(1)</script>", "description": "x",
             "author": "me", "downloads": 1, "server_side": "unknown", "project_type": "mod"},
        ]}),
    );
    let hits = h.svc.mods_search(&id, "beta", true, 0).await?;
    assert_eq!(hits.len(), 2);
    let beta = hits.first().expect("beta");
    assert!(beta.installed);
    assert!(!beta.title.contains('\u{7}'));
    assert!(beta.description.chars().count() <= 301);
    let alpha = hits.get(1).expect("alpha");
    assert!(!alpha.installed);
    assert_eq!(alpha.server_side, "unknown");
    Ok(())
}

#[tokio::test]
async fn the_list_reports_missing_and_changed_files_and_foreign_jars() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    h.svc.mods_install(&id, ALPHA, false).await?;
    fs::remove_file(h.svc.mods_dir(&id).join(format!("{BETA}-1.0.0.jar")))?;
    fs::write(
        h.svc.mods_dir(&id).join(format!("{ALPHA}-1.0.0.jar")),
        "tiny",
    )?;
    fs::write(h.svc.mods_dir(&id).join("mine.jar"), "m")?;
    fs::write(h.svc.mods_dir(&id).join("notes.txt"), "m")?;
    let list = h.svc.mods_list(&id).await?;
    let status = |p: &str| {
        list.mods
            .iter()
            .find(|m| m.project_id == p)
            .map(|m| m.status.clone())
    };
    assert_eq!(status(BETA).as_deref(), Some("missing"));
    assert_eq!(status(ALPHA).as_deref(), Some("size_changed"));
    assert_eq!(list.untracked, ["mine.jar"]);
    Ok(())
}

// ---- crash recovery --------------------------------------------------------------

fn journal(
    stage: Stage,
    adds: &[&str],
    removes: &[&str],
    upserts: Vec<crate::db::ModChange>,
) -> Journal {
    Journal {
        txn: "t".into(),
        stage,
        backup_id: None,
        adds: adds.iter().map(|s| (*s).to_owned()).collect(),
        removes: removes.iter().map(|s| (*s).to_owned()).collect(),
        remove_projects: Vec::new(),
        upserts,
    }
}

#[tokio::test]
async fn boot_recovery_undoes_a_change_that_was_cut_short() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    let mods = h.svc.mods_dir(&id);
    let root = h.svc.txn_root(&id);
    fs::create_dir_all(&mods)?;
    fs::write(mods.join("old.jar"), "old")?;
    txn::begin(&root)?;
    fs::write(txn::staging_dir(&root).join("new.jar"), "new")?;
    let j = journal(Stage::Applying, &["new.jar"], &["old.jar"], Vec::new());
    txn::write_journal(&root, &j)?;
    // Crash after both moves.
    txn::apply(&root, &mods, &j, &mut |_| true)?;
    assert_eq!(jars(&h, &id), ["new.jar"]);

    let report = h.svc.reconcile().await?;
    assert_eq!(report.servers_checked, 1);
    assert_eq!(jars(&h, &id), ["old.jar"]);
    assert_eq!(read(&mods.join("old.jar")), "old");
    assert!(txn_gone(&h, &id));
    assert!(h.svc.db().list_mods(&id)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn boot_recovery_finishes_the_records_of_a_committed_change() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    let mods = h.svc.mods_dir(&id);
    let root = h.svc.txn_root(&id);
    fs::create_dir_all(&mods)?;
    txn::begin(&root)?;
    fs::write(mods.join("beta.jar"), "jar")?;
    let row = crate::db::ModRow {
        server_id: id.clone(),
        project_id: BETA.into(),
        version_id: "b1".into(),
        title: "Beta Lib".into(),
        version_number: "1.0.0".into(),
        file_name: "beta.jar".into(),
        sha512: "0".repeat(128),
        size_bytes: 3,
        as_dependency: false,
        installed_unix: 1,
    };
    let change = crate::db::ModChange {
        row,
        requires: Vec::new(),
    };
    txn::write_journal(
        &root,
        &journal(Stage::Committed, &["beta.jar"], &[], vec![change]),
    )?;

    h.svc.reconcile().await?;
    assert_eq!(jars(&h, &id), ["beta.jar"], "committed files stay");
    assert_eq!(h.svc.db().list_mods(&id)?.len(), 1);
    assert!(txn_gone(&h, &id));
    // Idempotent.
    h.svc.reconcile().await?;
    assert_eq!(h.svc.db().list_mods(&id)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn leftover_staging_without_a_journal_is_just_removed() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    let root = h.svc.txn_root(&id);
    txn::begin(&root)?;
    fs::write(txn::staging_dir(&root).join("half.jar.part"), "x")?;
    h.svc.reconcile().await?;
    assert!(txn_gone(&h, &id));
    assert!(jars(&h, &id).is_empty());
    Ok(())
}

#[tokio::test]
async fn an_unreadable_journal_is_kept_and_blocks_nothing_else() -> TestResult {
    let h = harness();
    let id = stopped(&h).await?;
    let root = h.svc.txn_root(&id);
    txn::begin(&root)?;
    fs::write(root.join("journal.json"), "{ not json")?;
    h.svc.reconcile().await?;
    assert!(root.join("journal.json").exists(), "kept for inspection");
    Ok(())
}
