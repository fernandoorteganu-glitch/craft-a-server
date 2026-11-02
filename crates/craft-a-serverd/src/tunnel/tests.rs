// Test harness code: a failed setup or a timeout should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! Supervision tests: the real manager against fake agent *scripts* (a
//! shell script stands in for `playitd` and another for `playit-cli`) and a
//! canned HTTP fake. Nothing here contacts GitHub or playit.gg. Unix only
//! (the scripts need `/bin/sh`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use craft_a_server_core::tunnel::{TunnelInfo, TunnelState};

use super::fetch::fake::{Canned, FakeFetch};
use super::release::{PinnedAsset, RELEASE_BASE, Release};
use super::*;

const SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const CLAIM_URL: &str = "https://playit.gg/claim/abc123def0";

/// Fake `playitd`: records how it was called, opens its "socket", then runs
/// until killed. Creating `crash` in its directory makes it die at once.
const DAEMON_OK: &str = r#"#!/bin/sh
sock=""
echo "$@" > argv.txt
while [ $# -gt 0 ]; do
  case "$1" in
    --socket-path) sock="$2"; shift ;;
  esac
  shift
done
env > env.txt
echo started >> starts.log
if [ -f crash ]; then
  echo "fatal: simulated crash key 00112233445566778899aabbccddeeff00112233" >&2
  exit 3
fi
touch "$sock"
exec sleep 300
"#;

/// Fake `playit-cli --socket-path S setup`: prints the claim link, waits for
/// the test to "accept" (a file holding the secret), then writes the secret
/// file the way the real agent would (mode 0600).
const CLI_OK: &str = r#"#!/bin/sh
dir=$(dirname "$2")
echo "Open this link to finish setting up playit:"
echo "https://playit.gg/claim/abc123def0"
if [ ! -f "$dir/first_done" ] && [ -f "$dir/fail_first" ]; then
  touch "$dir/first_done"
  exit 1
fi
while [ ! -f "$dir/accept" ]; do sleep 0.02; done
umask 077
printf 'secret_key = "%s"\n' "$(cat "$dir/accept")" > "$dir/playit.toml"
exit 0
"#;

fn fast() -> Timings {
    Timings {
        tick: Duration::from_millis(20),
        backoff_base: Duration::from_millis(100),
        backoff_cap: Duration::from_millis(400),
        stable_after: Duration::from_secs(60),
        healthy_after: Duration::from_millis(150),
        rundata_pending: Duration::from_millis(40),
        rundata_running: Duration::from_millis(80),
        socket_wait: Duration::from_secs(5),
        claim_grace: Duration::from_millis(300),
        claim_lifetime: Duration::from_secs(60),
        stop_grace: Duration::from_secs(2),
    }
}

fn release_for(daemon: &str, cli: &str) -> Release {
    let mk = |name: &str, bytes: &str| PinnedAsset {
        name: name.to_owned(),
        sha256: sha256_hex(bytes.as_bytes()),
        size: bytes.len() as u64,
    };
    Release {
        version: "9.9.9".into(),
        base_url: RELEASE_BASE.to_owned(),
        assets: vec![
            mk("playit-linux-amd64", daemon),
            mk("playit-cli-linux-amd64", cli),
        ],
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    mgr: Arc<TunnelManager>,
    fetch: Arc<FakeFetch>,
    db: Db,
    release: Release,
}

fn rundata_ok(address_port: u16) -> Canned {
    Canned::Ok(
        format!(
            r#"{{"status":"success","data":{{"tunnels":[{{"tunnel_type":"minecraft-java","assigned_domain":"mine.craft.playit.gg","port":{{"from":{address_port}}},"local_ip":"127.0.0.1","local_port":25565}}]}}}}"#
        )
        .into_bytes(),
    )
}

fn harness_with(daemon: &str, cli: &str) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("tunnel");
    let release = release_for(daemon, cli);
    let fetch = Arc::new(FakeFetch::default());
    for (name, bytes) in [
        ("playit-linux-amd64", daemon),
        ("playit-cli-linux-amd64", cli),
    ] {
        let asset = release.asset(name).expect("asset");
        fetch.route(
            &release.url_of(asset),
            Canned::Ok(bytes.as_bytes().to_vec()),
        );
    }
    fetch.route(RUNDATA_URL, rundata_ok(12345));
    let db = Db::open_in_memory().expect("db");
    let settings = TunnelSettings {
        dirs: TunnelDirs::new(&root),
        mc_port: 25565,
        release: release.clone(),
        platform: Some(Platform::LinuxAmd64),
        rundata_url: RUNDATA_URL.to_owned(),
        timings: fast(),
    };
    let fetch_dyn: Arc<dyn HttpFetch> = fetch.clone();
    let mgr = Arc::new(TunnelManager::new(
        settings,
        db.clone(),
        fetch_dyn,
        Arc::new(|| 1_700_000_000),
    ));
    Harness {
        _dir: dir,
        root,
        mgr,
        fetch,
        db,
        release,
    }
}

fn harness() -> Harness {
    harness_with(DAEMON_OK, CLI_OK)
}

impl Harness {
    fn info(&self) -> TunnelInfo {
        self.mgr.info().expect("info")
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }

    fn starts(&self) -> usize {
        self.file("starts.log").lines().count()
    }

    fn pid(&self) -> Option<u32> {
        self.file("agent.pid").trim().parse().ok()
    }

    /// Runs supervisor passes until `done` holds for the state.
    async fn drive(&self, what: &str, done: impl Fn(&TunnelInfo) -> bool) {
        for _ in 0..600 {
            self.mgr.tick().await;
            if done(&self.info()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        panic!("timed out waiting for {what}; state: {:?}", self.info());
    }

    fn accept(&self) {
        std::fs::write(self.root.join("accept"), SECRET).expect("accept file");
    }
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| !s.contains(") Z"))
            .unwrap_or(false)
}

fn is_error(i: &TunnelInfo) -> bool {
    matches!(i.state, TunnelState::Error { .. })
}

fn error_text(i: &TunnelInfo) -> String {
    match &i.state {
        TunnelState::Error { reason } => reason.clone(),
        other => panic!("expected an error state, got {other:?}"),
    }
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn off_by_default_and_a_tick_does_nothing() {
    let h = harness();
    let info = h.info();
    assert!(!info.enabled);
    assert_eq!(info.state, TunnelState::NotConfigured);
    for _ in 0..5 {
        h.mgr.tick().await;
    }
    assert!(h.fetch.requests().is_empty(), "no network while off");
    assert!(!h.root.exists(), "no files while off");
}

#[tokio::test]
async fn enabling_needs_the_confirmation() {
    let h = harness();
    assert!(matches!(
        h.mgr.enable(false),
        Err(TunnelError::NotConfirmed)
    ));
    assert!(!h.info().enabled);
    assert_eq!(h.info().consent_unix, None);
    let info = h.mgr.enable(true).expect("enable");
    assert!(info.enabled);
    assert_eq!(info.consent_unix, Some(1_700_000_000));
}

#[tokio::test]
async fn unsupported_platforms_cannot_be_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let release = release_for(DAEMON_OK, CLI_OK);
    let settings = TunnelSettings {
        dirs: TunnelDirs::new(&dir.path().join("t")),
        mc_port: 25565,
        release,
        platform: None,
        rundata_url: RUNDATA_URL.to_owned(),
        timings: fast(),
    };
    let mgr = TunnelManager::new(
        settings,
        Db::open_in_memory().expect("db"),
        Arc::new(FakeFetch::default()),
        Arc::new(|| 0),
    );
    assert!(matches!(mgr.enable(true), Err(TunnelError::Unsupported)));
}

#[tokio::test]
async fn full_flow_from_claim_link_to_public_address() {
    let h = harness();
    h.mgr.enable(true).expect("enable");

    // The claim link appears once the agent is up.
    h.drive(
        "a claim link",
        |i| matches!(&i.state, TunnelState::Claiming { url, .. } if url == CLAIM_URL),
    )
    .await;
    let TunnelState::Claiming { code, .. } = h.info().state else {
        panic!("not claiming");
    };
    assert_eq!(code, "abc123def0");

    // The agent got file paths only, on a scrubbed environment.
    let argv = h.file("argv.txt");
    assert_eq!(
        argv.trim(),
        format!(
            "--secret-path {0}/playit.toml --socket-path {0}/playitd.sock",
            h.root.display()
        )
    );
    let env = h.file("env.txt");
    assert!(
        env.contains(&format!("HOME={}/home", h.root.display())),
        "{env}"
    );
    assert!(
        !env.contains("USER="),
        "environment must be scrubbed: {env}"
    );

    // The user accepts in the browser: the secret appears, then the address.
    h.accept();
    h.drive("the public address", |i| {
        matches!(&i.state, TunnelState::Running { address } if address == "mine.craft.playit.gg:12345")
    })
    .await;

    // The lookup authenticated with the secret, and only there.
    let auth: Vec<_> = h
        .fetch
        .requests()
        .into_iter()
        .filter_map(|r| r.authorization)
        .collect();
    assert!(!auth.is_empty());
    assert!(auth.iter().all(|a| a == &format!("agent-key {SECRET}")));
    assert!(!h.file("argv.txt").contains(SECRET));
    assert!(!h.file("env.txt").contains(SECRET));
    let events = h.db.recent_events(100).expect("events");
    assert!(
        events
            .iter()
            .all(|e| !e.detail.contains(SECRET) && !e.detail.contains("claim/")),
        "{events:?}"
    );

    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&h.root.join("playit.toml")), 0o600);
    assert_eq!(mode(&h.root), 0o700);
    assert_eq!(mode(&h.root.join("bin")), 0o700);

    // Turning it off stops the agent; the claim is kept.
    let pid = h.pid().expect("pid file");
    assert!(alive(pid));
    let off = h.mgr.disable(false).await.expect("disable");
    assert!(!off.enabled);
    assert_eq!(off.state, TunnelState::Claimed);
    assert!(!alive(pid), "the agent must be gone after disable");
    assert!(!h.root.join("agent.pid").exists());
    assert_eq!(
        off.consent_unix,
        Some(1_700_000_000),
        "consent is kept as a record"
    );

    // And a tick while off keeps it off.
    h.mgr.tick().await;
    assert_eq!(h.starts(), 1);
}

#[tokio::test]
async fn turning_it_on_again_reuses_the_claim_without_a_new_link() {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    h.drive("a claim link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    h.accept();
    h.drive("running", |i| {
        matches!(i.state, TunnelState::Running { .. })
    })
    .await;
    h.mgr.disable(false).await.expect("disable");
    let downloads = h.fetch.requests().len();

    let again = h.mgr.enable(true).expect("enable");
    assert_eq!(again.state, TunnelState::Claimed);
    h.drive("running again", |i| {
        matches!(i.state, TunnelState::Running { .. })
    })
    .await;
    assert_eq!(h.starts(), 2);
    // The binaries were verified on disk, not downloaded again.
    let new: Vec<_> = h.fetch.requests().split_off(downloads);
    assert!(new.iter().all(|r| r.url == RUNDATA_URL), "{new:?}");
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn forgetting_the_claim_removes_the_secret() {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    h.drive("a claim link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    h.accept();
    h.drive("claimed", |i| {
        matches!(i.state, TunnelState::Claimed | TunnelState::Running { .. })
    })
    .await;
    let off = h.mgr.disable(true).await.expect("disable");
    assert_eq!(off.state, TunnelState::NotConfigured);
    assert!(!h.root.join("playit.toml").exists());
}

#[tokio::test]
async fn a_crashing_agent_is_restarted_with_backoff_and_reported() {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    std::fs::create_dir_all(&h.root).expect("root");
    std::fs::write(h.root.join("crash"), "").expect("crash flag");

    // 1.5 s of 15 ms passes: without a backoff this would be ~100 starts.
    let end = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::Instant::now() < end {
        h.mgr.tick().await;
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    let starts = h.starts();
    assert!((3..=9).contains(&starts), "starts: {starts}");
    let info = h.info();
    assert!(is_error(&info), "{info:?}");
    let reason = error_text(&info);
    assert!(reason.contains("exit code 3"), "{reason}");
    assert!(reason.contains("fatal: simulated crash"), "{reason}");
    // The 40-hex-digit run in the agent's message never reaches the state.
    assert!(
        !reason.contains("00112233445566778899aabbccddeeff00112233"),
        "{reason}"
    );

    // Once the problem is gone the next restart recovers by itself.
    std::fs::remove_file(h.root.join("crash")).expect("remove flag");
    h.drive("a claim link after recovery", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn disabling_while_the_agent_is_running_stops_it_for_good() {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    h.drive("a claim link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    let pid = h.pid().expect("pid");
    assert!(alive(pid));
    h.mgr.disable(false).await.expect("disable");
    assert!(!alive(pid));
    for _ in 0..10 {
        h.mgr.tick().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.starts(), 1, "it must not come back");
    assert_eq!(h.info().state, TunnelState::NotConfigured);
}

#[tokio::test]
async fn daemon_shutdown_kills_the_agent_and_the_claim_program() {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    h.drive("a claim link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    let pid = h.pid().expect("pid");
    h.mgr.shutdown().await;
    assert!(!alive(pid));
    assert!(!h.root.join("agent.pid").exists());
}

#[tokio::test]
async fn the_run_loop_stops_on_request() {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    let (tx, rx) = watch::channel(false);
    let mgr = Arc::clone(&h.mgr);
    let task = tokio::spawn(async move { mgr.run(rx).await });
    for _ in 0..300 {
        if matches!(h.info().state, TunnelState::Claiming { .. }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(matches!(h.info().state, TunnelState::Claiming { .. }));
    let pid = h.pid().expect("pid");
    tx.send(true).expect("stop");
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("run loop must stop")
        .expect("join");
    assert!(!alive(pid));
}

#[tokio::test]
async fn a_failed_claim_program_is_retried_with_a_fresh_link() {
    let h = harness();
    std::fs::create_dir_all(&h.root).expect("root");
    std::fs::write(h.root.join("fail_first"), "").expect("flag");
    h.mgr.enable(true).expect("enable");
    h.drive("the claim to fail", is_error).await;
    assert!(error_text(&h.info()).contains("not completed"));
    h.drive("a fresh link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    h.accept();
    h.drive("running", |i| {
        matches!(i.state, TunnelState::Running { .. })
    })
    .await;
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn a_download_with_the_wrong_hash_is_never_installed_or_run() {
    let h = harness();
    // The CDN serves different bytes than the ones that were pinned.
    let asset = h.release.asset("playit-linux-amd64").expect("asset");
    let evil = DAEMON_OK.replacen("started", "startee", 1);
    assert_eq!(evil.len(), DAEMON_OK.len());
    h.fetch
        .route(&h.release.url_of(asset), Canned::Ok(evil.into_bytes()));
    h.mgr.enable(true).expect("enable");
    h.drive("an install error", is_error).await;
    let reason = error_text(&h.info());
    assert!(reason.contains("discarded"), "{reason}");
    assert!(!h.root.join("bin/9.9.9/playit-linux-amd64").exists());
    assert!(!h.root.join("pwned").exists());
    assert_eq!(h.starts(), 0);
}

#[tokio::test]
async fn placeholder_hashes_fail_closed_and_are_not_retried() {
    let h = harness();
    let mut release = h.release.clone();
    for a in &mut release.assets {
        a.sha256 = PLACEHOLDER_SHA256.to_owned();
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("t");
    let fetch = Arc::new(FakeFetch::default());
    let fetch_dyn: Arc<dyn HttpFetch> = fetch.clone();
    let mgr = TunnelManager::new(
        TunnelSettings {
            dirs: TunnelDirs::new(&root),
            mc_port: 25565,
            release,
            platform: Some(Platform::LinuxAmd64),
            rundata_url: RUNDATA_URL.to_owned(),
            timings: fast(),
        },
        Db::open_in_memory().expect("db"),
        fetch_dyn,
        Arc::new(|| 0),
    );
    mgr.enable(true).expect("enable");
    for _ in 0..5 {
        mgr.tick().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let info = mgr.info().expect("info");
    let reason = error_text(&info);
    assert!(reason.contains("no verified checksum"), "{reason}");
    assert!(fetch.requests().is_empty(), "nothing may be downloaded");
    assert!(!root.join("bin").exists(), "nothing may be written");
}

#[tokio::test]
async fn an_installed_file_that_changed_is_not_run() {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    h.drive("a claim link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    h.mgr.disable(false).await.expect("disable");
    assert_eq!(h.starts(), 1);

    // Someone swaps the binary on disk while the tunnel is off.
    let bin = h.root.join("bin/9.9.9/playit-linux-amd64");
    std::fs::write(&bin, "#!/bin/sh\necho pwned > pwned\nexec sleep 300\n").expect("tamper");
    h.mgr.enable(true).expect("enable");
    h.mgr.tick().await;
    let reason = error_text(&h.info());
    assert!(
        reason.contains("does not match the pinned checksum"),
        "{reason}"
    );
    assert!(
        !h.root.join("pwned").exists(),
        "the tampered file must never run"
    );
    assert_eq!(h.starts(), 1);

    // The next pass reinstalls the pinned bytes and carries on.
    h.drive("a claim link again", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    assert!(!h.root.join("pwned").exists());
    assert_eq!(h.starts(), 2);
    h.mgr.shutdown().await;
}

async fn claimed_harness() -> Harness {
    let h = harness();
    h.mgr.enable(true).expect("enable");
    h.drive("a claim link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    h.accept();
    h.drive("running", |i| {
        matches!(i.state, TunnelState::Running { .. })
    })
    .await;
    h
}

#[tokio::test]
async fn a_tunnel_that_forwards_off_this_machine_is_an_error_not_an_address() {
    let h = claimed_harness().await;
    h.fetch.route(
        RUNDATA_URL,
        Canned::Ok(
            br#"{"status":"success","data":{"tunnels":[{"tunnel_type":"minecraft-java","assigned_domain":"lan.craft.playit.gg","port":{"from":1},"local_ip":"192.168.1.5","local_port":25565}]}}"#
                .to_vec(),
        ),
    );
    h.drive("an origin error", is_error).await;
    assert!(error_text(&h.info()).contains("127.0.0.1:25565"));
    // Fixing the tunnel in the dashboard clears it.
    h.fetch.route(RUNDATA_URL, rundata_ok(4242));
    h.drive("the fixed address", |i| {
        matches!(&i.state, TunnelState::Running { address } if address == "mine.craft.playit.gg:4242")
    })
    .await;
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn no_matching_tunnel_leaves_the_state_at_claimed() {
    let h = claimed_harness().await;
    h.fetch.route(
        RUNDATA_URL,
        Canned::Ok(br#"{"status":"success","data":{"tunnels":[]}}"#.to_vec()),
    );
    h.drive("claimed without a tunnel", |i| {
        i.state == TunnelState::Claimed
    })
    .await;
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn rejected_credentials_and_unreadable_answers_are_reported() {
    let h = claimed_harness().await;
    h.fetch.route(RUNDATA_URL, Canned::Status(401));
    h.drive("a rejection", is_error).await;
    assert!(error_text(&h.info()).contains("rejected"));
    h.fetch
        .route(RUNDATA_URL, Canned::Ok(b"<html>nope</html>".to_vec()));
    // Still an error; now about the format.
    h.drive(
        "a format error",
        |i| matches!(&i.state, TunnelState::Error { reason } if reason.contains("format")),
    )
    .await;
    // playit.gg being unreachable changes nothing.
    h.fetch.route(RUNDATA_URL, rundata_ok(1));
    h.drive("running again", |i| {
        matches!(i.state, TunnelState::Running { .. })
    })
    .await;
    h.fetch.route(RUNDATA_URL, Canned::Network);
    for _ in 0..15 {
        h.mgr.tick().await;
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    assert!(matches!(h.info().state, TunnelState::Running { .. }));
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn a_hostile_address_from_the_api_never_reaches_the_state() {
    let h = claimed_harness().await;
    h.fetch.route(
        RUNDATA_URL,
        Canned::Ok(
            br#"{"status":"success","data":{"tunnels":[{"tunnel_type":"minecraft-java","assigned_domain":"<script>alert(1)</script>.gg","port":{"from":25565},"local_ip":"127.0.0.1","local_port":25565}]}}"#
                .to_vec(),
        ),
    );
    h.drive("claimed without an address", |i| {
        i.state == TunnelState::Claimed
    })
    .await;
    let dump = format!("{:?}", h.db.get_tunnel().expect("row"));
    assert!(!dump.contains("script"));
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn losing_the_secret_file_starts_a_new_claim() {
    let h = claimed_harness().await;
    std::fs::remove_file(h.root.join("playit.toml")).expect("remove secret");
    std::fs::remove_file(h.root.join("accept")).expect("remove accept");
    h.drive("a new claim link", |i| {
        matches!(i.state, TunnelState::Claiming { .. })
    })
    .await;
    h.mgr.shutdown().await;
}

#[tokio::test]
async fn a_leftover_agent_from_a_crashed_daemon_is_stopped_at_start() {
    // A copy of `sleep` stands in for the old agent binary.
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = dir.path().join("fake-playitd");
    std::fs::copy("/bin/sleep", &exe).expect("copy sleep");
    let mut child = std::process::Command::new(&exe)
        .arg("300")
        .spawn()
        .expect("spawn");
    let pid = child.id();
    // Reap it as soon as it dies, like init would for a real orphan.
    let waiter = std::thread::spawn(move || child.wait());
    let pid_file = dir.path().join("agent.pid");
    std::fs::write(&pid_file, format!("{pid}\n")).expect("pid file");
    assert!(alive(pid));
    assert!(process::reap_stale(&pid_file, &exe).await);
    let _ = waiter.join();
    assert!(!alive(pid));
    assert!(!pid_file.exists());

    // A pid file that points at some other program is left alone.
    let mut other = std::process::Command::new("/bin/sleep")
        .arg("300")
        .spawn()
        .expect("spawn");
    std::fs::write(&pid_file, format!("{}\n", other.id())).expect("pid file");
    assert!(!process::reap_stale(&pid_file, &exe).await);
    assert!(alive(other.id()));
    let _ = other.kill();
    let _ = other.wait();
}
