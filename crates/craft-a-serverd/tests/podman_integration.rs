//! Integration tests against REAL rootless Podman (Linux only, e.g. WSL Debian).
//!
//! Ignored by default and a no-op unless `CRAFT_PODMAN_TESTS=1`:
//!
//! ```sh
//! CRAFT_PODMAN_TESTS=1 cargo test -p craft-a-serverd --test podman_integration -- --ignored --nocapture
//! ```
//!
//! Run them as the unprivileged service user (with `XDG_RUNTIME_DIR` set). Images
//! needed: `docker.io/itzg/minecraft-server:latest` (override `CRAFT_TEST_IMAGE`)
//! and `docker.io/library/alpine:latest` (override `CRAFT_TEST_PROBE_IMAGE`).
//! Set `CRAFT_TEST_OFFLINE=1` to skip the "internet is open" assertion.

#![cfg(unix)]

use std::error::Error;
use std::time::{Duration, Instant};

use craft_a_serverd::runtime::{
    ContainerRuntime, ContainerState, EgressPolicy, PodmanCli, RuntimeError, ServerContainerSpec,
    mc_status, pod_name,
};

type TestResult = Result<(), Box<dyn Error>>;

fn enabled() -> bool {
    let on = std::env::var("CRAFT_PODMAN_TESTS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipping: CRAFT_PODMAN_TESTS != 1");
    }
    on
}

fn image() -> String {
    std::env::var("CRAFT_TEST_IMAGE")
        .unwrap_or_else(|_| "docker.io/itzg/minecraft-server:latest".to_owned())
}

fn probe_image() -> String {
    std::env::var("CRAFT_TEST_PROBE_IMAGE")
        .unwrap_or_else(|_| "docker.io/library/alpine:latest".to_owned())
}

fn unique_id(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("it-{tag}-{}-{nanos:x}", std::process::id())
}

fn spec(
    id: &str,
    data: &std::path::Path,
    port: u16,
) -> Result<ServerContainerSpec, Box<dyn Error>> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // SAFETY-free way to get the numeric ids: read /proc/self/status.
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |name: &str| -> Option<u32> {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
    };
    Ok(ServerContainerSpec {
        id: id.to_owned(),
        mc_version: "LATEST".into(),
        image: image(),
        data_dir: data.to_path_buf(),
        host_port: port,
        memory_limit_mib: 3072,
        heap_mib: 2048,
        cpus: 2.0,
        pids_limit: 512,
        uid: field("Uid:").ok_or("no uid")?,
        gid: field("Gid:").ok_or("no gid")?,
        stop_timeout_secs: 30,
        eula_accepted: true,
        rcon_password: format!("{nanos:032x}").chars().take(32).collect(),
        view_distance: None,
    })
}

/// Runs `podman <args>` and returns (success, stdout, stderr).
async fn podman(args: &[&str]) -> Result<(bool, String, String), Box<dyn Error>> {
    let out = tokio::process::Command::new("podman")
        .args(args)
        .output()
        .await?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

async fn infra_pid(id: &str) -> Result<String, Box<dyn Error>> {
    let (_, infra, _) = podman(&[
        "pod",
        "inspect",
        "--format",
        "{{.InfraContainerID}}",
        &pod_name(id),
    ])
    .await?;
    let (_, pid, _) = podman(&["inspect", "--format", "{{.State.Pid}}", infra.trim()]).await?;
    Ok(pid.trim().to_owned())
}

async fn egress_listing(id: &str) -> Result<String, Box<dyn Error>> {
    let pid = infra_pid(id).await?;
    let (ok, out, err) = podman(&[
        "unshare",
        "nsenter",
        "-t",
        &pid,
        "-n",
        "/usr/sbin/nft",
        "list",
        "table",
        "inet",
        "craft_egress",
    ])
    .await?;
    if !ok {
        return Err(format!("nft list failed: {err}").into());
    }
    Ok(out)
}

/// Packets counted by the IPv4 drop rule.
fn dropped_packets(listing: &str) -> u64 {
    listing
        .lines()
        .filter(|l| l.contains("ip daddr {") && l.contains("counter packets"))
        .filter_map(|l| {
            let rest = l.split("counter packets").nth(1)?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        })
        .sum()
}

/// TCP connect probe from a hardened alpine container inside the server's pod.
async fn probe(id: &str, ip: &str, port: &str) -> Result<bool, Box<dyn Error>> {
    let img = probe_image();
    let pod = pod_name(id);
    let (ok, _, _) = podman(&[
        "run",
        "--rm",
        "--pod",
        &pod,
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt",
        "no-new-privileges",
        &img,
        "nc",
        "-z",
        "-w",
        "3",
        ip,
        port,
    ])
    .await?;
    Ok(ok)
}

async fn wait_until<F, Fut>(what: &str, timeout: Duration, mut f: F) -> TestResult
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!("timed out waiting for {what}").into())
}

#[tokio::test]
#[ignore = "needs rootless podman; set CRAFT_PODMAN_TESTS=1 and pass --ignored"]
async fn full_lifecycle_with_egress_containment() -> TestResult {
    if !enabled() {
        return Ok(());
    }
    let rt = PodmanCli::default();
    let id = unique_id("life");
    let data = tempfile::tempdir()?;
    let port = 25_600 + u16::try_from(std::process::id() % 300)?;
    let mut spec = spec(&id, data.path(), port)?;
    spec.view_distance = Some(6);

    println!("podman version: {}", rt.version().await?);
    rt.remove(&id).await?;

    let body = async {
        rt.create_and_start(&spec).await?;
        assert!(matches!(
            rt.create_and_start(&spec).await,
            Err(RuntimeError::AlreadyExists(_))
        ));

        // Egress table present in the pod netns.
        let listing = egress_listing(&id).await?;
        assert!(listing.contains("table inet craft_egress"), "{listing}");

        // Reconciliation sees the pod.
        let managed = rt.list_managed().await?;
        assert!(
            managed.iter().any(|p| p.server_id == id),
            "list_managed: {managed:?}"
        );

        // Probe matrix: LAN-ish destinations are dropped (counter moves),
        // the internet stays open.
        let before = dropped_packets(&listing);
        for ip in ["10.99.0.2", "192.168.1.1", "169.254.169.254", "172.16.0.1"] {
            assert!(!probe(&id, ip, "80").await?, "{ip} must be unreachable");
        }
        let after = dropped_packets(&egress_listing(&id).await?);
        assert!(
            after > before,
            "drop counter did not move ({before} -> {after}): packets never reached the rule"
        );
        if std::env::var("CRAFT_TEST_OFFLINE").is_err() {
            assert!(
                probe(&id, "1.1.1.1", "443").await?,
                "internet must stay reachable"
            );
        }

        // The server comes up: SLP and RCON both work.
        wait_until("server status", Duration::from_secs(240), || async {
            mc_status("127.0.0.1", port, Duration::from_secs(3))
                .await
                .is_ok()
        })
        .await?;
        let info = rt.inspect(&id).await?;
        assert_eq!(info.state, ContainerState::Running);
        assert!(!info.oom_killed);
        let reply = rt.rcon(&id, "list").await?;
        assert!(reply.to_lowercase().contains("players"), "rcon: {reply}");

        // The chosen view distance reached the game's own settings.
        let props = std::fs::read_to_string(data.path().join("server.properties"))?;
        assert!(
            props.lines().any(|l| l.trim() == "view-distance=6"),
            "server.properties: {props}"
        );

        // Resource readings come from podman and parse.
        let reading = rt
            .stats(&id)
            .await?
            .ok_or("no stats for a running server")?;
        println!("stats: {reading:?}");
        assert!(reading.mem_used_bytes > 50 * 1024 * 1024, "{reading:?}");
        assert!(reading.pids > 1, "{reading:?}");
        assert!(reading.cpu_time > Duration::ZERO, "{reading:?}");

        // Logs.
        assert!(!rt.logs_tail(&id, 20).await?.is_empty());
        let mut rx = rt.logs_follow(&id).await?;
        rt.rcon(&id, "say integration-marker").await?;
        let found = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(line) = rx.recv().await {
                if line.contains("integration-marker") {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(found, "followed log never showed the rcon say");
        drop(rx);

        // Stop, then remove (twice: idempotent).
        rt.stop(&id, Duration::from_secs(30)).await?;
        assert_eq!(rt.stats(&id).await?, None, "no reading once it has stopped");
        assert_eq!(
            rt.inspect(&id).await?.state,
            ContainerState::Exited { code: 0 }
        );
        Ok::<(), Box<dyn Error>>(())
    }
    .await;

    // Always clean up, then report the body's result.
    rt.remove(&id).await?;
    rt.remove(&id).await?;
    assert_eq!(rt.inspect(&id).await?.state, ContainerState::Missing);
    assert!(rt.list_managed().await?.iter().all(|p| p.server_id != id));
    body
}

#[tokio::test]
#[ignore = "needs rootless podman; set CRAFT_PODMAN_TESTS=1 and pass --ignored"]
async fn fails_closed_when_rules_cannot_be_applied() -> TestResult {
    if !enabled() {
        return Ok(());
    }
    let data = tempfile::tempdir()?;

    // 1. nft rejects the ruleset.
    let id = unique_id("bad");
    let broken = EgressPolicy::from_ruleset(
        "table inet craft_egress { chain out { type filter hook output priority 0; bogus } }",
    )?;
    let rt = PodmanCli::default().with_egress(broken);
    let s = spec(&id, data.path(), 25_990)?;
    let r = rt.create_and_start(&s).await;
    assert!(
        matches!(&r, Err(RuntimeError::Egress(m)) if m.contains("syntax") || m.contains("Error")),
        "{r:?}"
    );
    let (exists, _, _) = podman(&["pod", "exists", &pod_name(&id)]).await?;
    assert!(!exists, "pod must be removed after egress failure");
    let (cexists, _, _) =
        podman(&["container", "exists", &format!("{}-mc", pod_name(&id))]).await?;
    assert!(!cexists, "game container must never have been created");

    // 2. nft succeeds but the verified table is absent.
    let id = unique_id("miss");
    let wrong = EgressPolicy::from_ruleset("# table inet craft_egress\ntable inet other { }\n")?;
    let rt = PodmanCli::default().with_egress(wrong);
    let s = spec(&id, data.path(), 25_991)?;
    let r = rt.create_and_start(&s).await;
    assert!(matches!(r, Err(RuntimeError::Egress(_))), "{r:?}");
    let (exists, _, _) = podman(&["pod", "exists", &pod_name(&id)]).await?;
    assert!(!exists);
    Ok(())
}
