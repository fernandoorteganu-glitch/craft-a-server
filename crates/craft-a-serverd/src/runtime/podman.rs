//! [`PodmanCli`]: the real [`ContainerRuntime`], driving the `podman` binary with
//! argument vectors only (never a shell string). The daemon runs as the service
//! user, so rootless podman is invoked directly.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::egress::{EGRESS_TABLE, EgressPolicy};
use super::spec::{ServerContainerSpec, build_pod_create_args, build_run_args, validate_id};
use super::{
    ContainerInfo, ContainerRuntime, ContainerState, ContainerStats, LABEL_ID, LogReceiver,
    ManagedPod, RuntimeError, container_name, pod_name, rcon_words, stats,
};

/// Longest log line delivered; the rest of an over-long line is dropped.
pub const LOG_LINE_CAP: usize = 8 * 1024;
/// Bound of the log channel; a slow consumer back-pressures the reader.
pub const LOG_CHANNEL_BOUND: usize = 1024;
/// Most lines `logs_tail` will ask for.
pub const MAX_TAIL_LINES: usize = 10_000;
const STDERR_CAP: usize = 2000;

/// Per-call timeouts.
#[derive(Debug, Clone)]
pub struct Timeouts {
    /// Quick queries (inspect, exists, version, rcon, list).
    pub query: Duration,
    /// Pod create/start/remove and nft application.
    pub pod_ops: Duration,
    /// `podman run` (may need to create the container; pulls are done elsewhere).
    pub run: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            query: Duration::from_secs(30),
            pod_ops: Duration::from_secs(60),
            run: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PodmanCli {
    binary: String,
    /// `nft` as run inside the pod netns; defaults to `/usr/sbin/nft` when it
    /// exists (sbin is often missing from a service's PATH), else `nft`.
    nft_binary: String,
    egress: EgressPolicy,
    timeouts: Timeouts,
}

impl Default for PodmanCli {
    fn default() -> Self {
        Self::new("podman")
    }
}

#[derive(Debug)]
struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Out {
    fn success(&self) -> bool {
        self.code == Some(0)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.trim().to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", s.get(..end).unwrap_or_default().trim())
}

fn default_nft_binary() -> String {
    const SBIN: &str = "/usr/sbin/nft";
    if std::path::Path::new(SBIN).exists() {
        SBIN.to_owned()
    } else {
        "nft".to_owned()
    }
}

fn is_no_such(stderr: &str) -> bool {
    let l = stderr.to_ascii_lowercase();
    l.contains("no such") || l.contains("no container with name or id") || l.contains("not found")
}

impl PodmanCli {
    /// `binary` may be a bare name (resolved via PATH) or an absolute path.
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            nft_binary: default_nft_binary(),
            egress: EgressPolicy::default(),
            timeouts: Timeouts::default(),
        }
    }

    pub fn with_nft_binary(mut self, nft: impl Into<String>) -> Self {
        self.nft_binary = nft.into();
        self
    }

    pub fn with_egress(mut self, egress: EgressPolicy) -> Self {
        self.egress = egress;
        self
    }

    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Runs podman and captures output. A non-zero exit is NOT an error here.
    async fn exec(
        &self,
        op: &str,
        args: &[String],
        stdin: Option<&[u8]>,
        env: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Out, RuntimeError> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args)
            .envs(env.iter().copied())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let spawn_err = |e: std::io::Error| RuntimeError::Spawn {
            op: op.to_owned(),
            message: e.to_string(),
        };
        let mut child = cmd.spawn().map_err(spawn_err)?;
        let input = stdin.map(<[u8]>::to_vec);
        let io = async {
            if let (Some(mut pipe), Some(data)) = (child.stdin.take(), input) {
                pipe.write_all(&data).await?;
                pipe.shutdown().await?;
            }
            child.wait_with_output().await
        };
        let output = match tokio::time::timeout(timeout, io).await {
            Ok(r) => r.map_err(|e| RuntimeError::Spawn {
                op: op.to_owned(),
                message: e.to_string(),
            })?,
            Err(_) => {
                return Err(RuntimeError::Timeout {
                    op: op.to_owned(),
                    secs: timeout.as_secs(),
                });
            }
        };
        Ok(Out {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Like [`exec`](Self::exec) but a non-zero exit becomes `CommandFailed`.
    async fn run_ok(
        &self,
        op: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Out, RuntimeError> {
        let out = self.exec(op, args, None, &[], timeout).await?;
        if out.success() {
            Ok(out)
        } else {
            Err(failed(op, &out))
        }
    }

    async fn exists(&self, kind: &str, name: &str) -> Result<bool, RuntimeError> {
        let args = vec![kind.to_owned(), "exists".into(), name.to_owned()];
        let out = self
            .exec("exists", &args, None, &[], self.timeouts.query)
            .await?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed("exists", &out)),
        }
    }

    async fn require_container(&self, id: &str) -> Result<String, RuntimeError> {
        validate_id(id)?;
        let name = container_name(id);
        if self.exists("container", &name).await? {
            Ok(name)
        } else {
            Err(RuntimeError::NotFound(format!("container {name}")))
        }
    }

    async fn remove_pod_best_effort(&self, name: &str) {
        let args: Vec<String> = ["pod", "rm", "-f", "-t", "0", "--ignore", name]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        if let Err(e) = self.run_ok("pod rm", &args, self.timeouts.pod_ops).await {
            tracing::warn!(pod = name, error = %e, "cleanup of pod failed");
        }
    }

    async fn infra_pid(&self, pod: &str) -> Result<u32, RuntimeError> {
        let out = self
            .run_ok(
                "pod inspect",
                &["pod".into(), "inspect".into(), pod.to_owned()],
                self.timeouts.query,
            )
            .await?;
        // Podman 5 prints a one-element array; older versions a bare object.
        let infos: OneOrMany<PodInspect> = parse_json(&out.stdout)?;
        let infra_container_id = match infos {
            OneOrMany::One(p) => p.infra_container_id,
            OneOrMany::Many(v) => v
                .into_iter()
                .next()
                .map(|p| p.infra_container_id)
                .unwrap_or_default(),
        };
        if infra_container_id.is_empty() {
            return Err(RuntimeError::Egress("pod has no infra container".into()));
        }
        let out = self
            .run_ok(
                "container inspect",
                &["container".into(), "inspect".into(), infra_container_id],
                self.timeouts.query,
            )
            .await?;
        let infos: Vec<ContainerInspect> = parse_json(&out.stdout)?;
        let pid = infos.first().map_or(0, |c| c.state.pid);
        u32::try_from(pid).ok().filter(|p| *p > 1).ok_or_else(|| {
            RuntimeError::Egress(format!("infra container has no usable pid ({pid})"))
        })
    }

    /// Applies the egress ruleset into the pod netns and verifies the table is
    /// present. Any failure is an `Egress` error (the caller removes the pod).
    async fn apply_egress(&self, pid: u32) -> Result<(), RuntimeError> {
        let pid = pid.to_string();
        let nsenter = |tail: &[&str]| -> Vec<String> {
            let mut v: Vec<String> = ["unshare", "nsenter", "-t", &pid, "-n", &self.nft_binary]
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            v.extend(tail.iter().map(|s| (*s).to_owned()));
            v
        };
        let apply = self
            .exec(
                "nft apply",
                &nsenter(&["-f", "-"]),
                Some(self.egress.ruleset().as_bytes()),
                &[],
                self.timeouts.pod_ops,
            )
            .await
            .map_err(|e| RuntimeError::Egress(e.to_string()))?;
        if !apply.success() {
            return Err(RuntimeError::Egress(format!(
                "nft -f failed (exit {:?}): {}",
                apply.code,
                truncate(&apply.stderr, STDERR_CAP)
            )));
        }
        let verify = self
            .exec(
                "nft verify",
                &nsenter(&["list", "table", "inet", EGRESS_TABLE]),
                None,
                &[],
                self.timeouts.pod_ops,
            )
            .await
            .map_err(|e| RuntimeError::Egress(e.to_string()))?;
        let needle = format!("table inet {EGRESS_TABLE}");
        if !verify.success() || !verify.stdout.contains(&needle) {
            return Err(RuntimeError::Egress(format!(
                "egress table not present after apply (exit {:?}): {}",
                verify.code,
                truncate(&verify.stderr, STDERR_CAP)
            )));
        }
        Ok(())
    }

    async fn start_in_pod(
        &self,
        spec: &ServerContainerSpec,
        pod: &str,
        run_args: &[String],
    ) -> Result<(), RuntimeError> {
        self.run_ok(
            "pod start",
            &["pod".into(), "start".into(), pod.to_owned()],
            self.timeouts.pod_ops,
        )
        .await?;
        let pid = self.infra_pid(pod).await?;
        self.apply_egress(pid).await?;
        // Only now, with the rules verified, may the game process exist.
        let out = self
            .exec(
                "run",
                run_args,
                None,
                &[("RCON_PASSWORD", spec.rcon_password.as_str())],
                self.timeouts.run,
            )
            .await?;
        if out.success() {
            Ok(())
        } else {
            Err(failed("run", &out))
        }
    }
}

fn failed(op: &str, out: &Out) -> RuntimeError {
    RuntimeError::CommandFailed {
        op: op.to_owned(),
        code: out.code,
        stderr: truncate(&out.stderr, STDERR_CAP),
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T, RuntimeError> {
    serde_json::from_str(s).map_err(|e| RuntimeError::Parse(e.to_string()))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    Many(Vec<T>),
    One(T),
}

#[derive(Deserialize)]
struct PodInspect {
    #[serde(rename = "InfraContainerID", default)]
    infra_container_id: String,
}

#[derive(Deserialize)]
struct ContainerInspect {
    #[serde(rename = "State")]
    state: InspectState,
}

#[derive(Deserialize)]
struct InspectState {
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "ExitCode", default)]
    exit_code: i32,
    #[serde(rename = "OOMKilled", default)]
    oom_killed: bool,
    #[serde(rename = "StartedAt", default)]
    started_at: String,
    #[serde(rename = "Pid", default)]
    pid: i64,
}

#[derive(Deserialize)]
struct PodPs {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "Labels", default)]
    labels: Option<HashMap<String, String>>,
    #[serde(rename = "Containers", default)]
    containers: Option<Vec<PodPsContainer>>,
}

#[derive(Deserialize)]
struct PodPsContainer {
    #[serde(rename = "Names", default)]
    names: String,
}

fn container_info(raw: &InspectState) -> ContainerInfo {
    let state = match raw.status.as_str() {
        "running" => ContainerState::Running,
        "exited" | "stopped" => ContainerState::Exited {
            code: raw.exit_code,
        },
        other => ContainerState::Other(other.to_owned()),
    };
    let started_at =
        Some(raw.started_at.clone()).filter(|s| !s.is_empty() && !s.starts_with("0001-01-01"));
    ContainerInfo {
        state,
        oom_killed: raw.oom_killed,
        started_at,
    }
}

/// Reads one `\n`-terminated line, keeping at most `cap` bytes of it (the rest
/// is discarded, never buffered). `Ok(None)` at end of stream.
async fn read_capped_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut line: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut any = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            break;
        }
        any = true;
        let newline = buf.iter().position(|b| *b == b'\n');
        let take = newline.unwrap_or(buf.len());
        let room = cap.saturating_sub(line.len());
        let keep = take.min(room);
        line.extend_from_slice(buf.get(..keep).unwrap_or_default());
        if keep < take {
            truncated = true;
        }
        let consumed = newline.map_or(buf.len(), |i| i + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(finish_line(line, truncated)));
        }
    }
    if any {
        Ok(Some(finish_line(line, truncated)))
    } else {
        Ok(None)
    }
}

fn finish_line(mut bytes: Vec<u8>, truncated: bool) -> String {
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let mut s = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        s.push_str("...[line truncated]");
    }
    s
}

async fn pump_lines<R: tokio::io::AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(reader);
    while let Ok(Some(line)) = read_capped_line(&mut reader, LOG_LINE_CAP).await {
        if is_podman_noise(&line) {
            continue;
        }
        if tx.send(line).await.is_err() {
            break;
        }
    }
}

/// Podman's own logfmt warnings (cgroup manager, lingering...) share stderr
/// with the container's log; they are not the game's output.
fn is_podman_noise(line: &str) -> bool {
    line.starts_with("time=\"") && line.contains(" level=warning msg=")
}

impl ContainerRuntime for PodmanCli {
    async fn create_and_start(&self, spec: &ServerContainerSpec) -> Result<(), RuntimeError> {
        // Validate everything (incl. the EULA) before any side effect.
        let pod_args = build_pod_create_args(spec)?;
        let run_args = build_run_args(spec)?;
        let pod = pod_name(&spec.id);
        if self.exists("pod", &pod).await? {
            return Err(RuntimeError::AlreadyExists(format!("pod {pod}")));
        }
        if let Err(e) = self
            .run_ok("pod create", &pod_args, self.timeouts.pod_ops)
            .await
        {
            self.remove_pod_best_effort(&pod).await;
            return Err(e);
        }
        match self.start_in_pod(spec, &pod, &run_args).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Fail closed: never leave a pod (or game container) behind.
                self.remove_pod_best_effort(&pod).await;
                Err(e)
            }
        }
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<(), RuntimeError> {
        let name = self.require_container(id).await?;
        let args = vec![
            "stop".to_owned(),
            "-t".into(),
            grace.as_secs().to_string(),
            name,
        ];
        let timeout = grace + Duration::from_secs(30);
        let out = self.exec("stop", &args, None, &[], timeout).await?;
        if out.success() {
            Ok(())
        } else if is_no_such(&out.stderr) {
            Err(RuntimeError::NotFound(format!("container of {id}")))
        } else {
            Err(failed("stop", &out))
        }
    }

    async fn kill(&self, id: &str) -> Result<(), RuntimeError> {
        let name = self.require_container(id).await?;
        let out = self
            .exec(
                "kill",
                &["kill".to_owned(), name],
                None,
                &[],
                self.timeouts.query,
            )
            .await?;
        let l = out.stderr.to_ascii_lowercase();
        if out.success() || l.contains("state improper") || l.contains("not running") {
            Ok(())
        } else if is_no_such(&out.stderr) {
            Err(RuntimeError::NotFound(format!("container of {id}")))
        } else {
            Err(failed("kill", &out))
        }
    }

    async fn remove(&self, id: &str) -> Result<(), RuntimeError> {
        validate_id(id)?;
        let pod = pod_name(id);
        let args: Vec<String> = ["pod", "rm", "-f", "-t", "0", "--ignore", pod.as_str()]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let out = self
            .exec("pod rm", &args, None, &[], self.timeouts.pod_ops)
            .await?;
        if !out.success() && !is_no_such(&out.stderr) {
            return Err(failed("pod rm", &out));
        }
        // A container that outlived its pod (should not happen) is removed too.
        let name = container_name(id);
        let args: Vec<String> = ["rm", "-f", "-t", "0", "--ignore", name.as_str()]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let out = self
            .exec("rm", &args, None, &[], self.timeouts.pod_ops)
            .await?;
        if !out.success() && !is_no_such(&out.stderr) {
            return Err(failed("rm", &out));
        }
        Ok(())
    }

    async fn inspect(&self, id: &str) -> Result<ContainerInfo, RuntimeError> {
        validate_id(id)?;
        let name = container_name(id);
        let out = self
            .exec(
                "inspect",
                &["container".to_owned(), "inspect".into(), name],
                None,
                &[],
                self.timeouts.query,
            )
            .await?;
        if !out.success() {
            return if is_no_such(&out.stderr) {
                Ok(ContainerInfo::missing())
            } else {
                Err(failed("inspect", &out))
            };
        }
        let infos: Vec<ContainerInspect> = parse_json(&out.stdout)?;
        Ok(infos
            .first()
            .map_or_else(ContainerInfo::missing, |c| container_info(&c.state)))
    }

    async fn logs_follow(&self, id: &str) -> Result<LogReceiver, RuntimeError> {
        let name = self.require_container(id).await?;
        let mut child = Command::new(&self.binary)
            .args(["logs", "--follow", "--tail", "0", name.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RuntimeError::Spawn {
                op: "logs".into(),
                message: e.to_string(),
            })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::channel(LOG_CHANNEL_BOUND);
        tokio::spawn(async move {
            let t1 = tx.clone();
            let t2 = tx.clone();
            let readers = async {
                tokio::join!(
                    async {
                        if let Some(s) = stdout {
                            pump_lines(s, t1).await;
                        }
                    },
                    async {
                        if let Some(s) = stderr {
                            pump_lines(s, t2).await;
                        }
                    }
                )
            };
            // Stop when the consumer goes away or the log ends.
            tokio::select! {
                () = tx.closed() => {}
                _ = readers => {}
            }
            let _ = child.kill().await;
        });
        Ok(rx)
    }

    async fn logs_tail(&self, id: &str, lines: usize) -> Result<Vec<String>, RuntimeError> {
        let name = self.require_container(id).await?;
        let n = lines.min(MAX_TAIL_LINES);
        let out = self
            .run_ok(
                "logs",
                &["logs".to_owned(), "--tail".into(), n.to_string(), name],
                self.timeouts.query,
            )
            .await?;
        // podman interleaves the container's stdout and stderr streams; the
        // relative order of the two is not guaranteed.
        let mut all: Vec<String> = Vec::new();
        for text in [&out.stdout, &out.stderr] {
            for line in text.lines().filter(|l| !is_podman_noise(l)) {
                all.push(truncate(line, LOG_LINE_CAP));
            }
        }
        Ok(all)
    }

    async fn rcon(&self, id: &str, command: &str) -> Result<String, RuntimeError> {
        let words = rcon_words(command)?;
        let name = self.require_container(id).await?;
        let mut args = vec!["exec".to_owned(), name, "rcon-cli".into()];
        args.extend(words);
        let out = self.run_ok("rcon", &args, self.timeouts.query).await?;
        Ok(truncate(&out.stdout, 64 * 1024))
    }

    async fn stats(&self, id: &str) -> Result<Option<ContainerStats>, RuntimeError> {
        validate_id(id)?;
        let args: Vec<String> = [
            "stats",
            "--no-stream",
            "--format",
            "json",
            container_name(id).as_str(),
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let out = self
            .exec("stats", &args, None, &[], self.timeouts.query)
            .await?;
        if !out.success() {
            // A container that is not running is "no reading", not a fault.
            return if is_no_such(&out.stderr) || out.stderr.contains("not running") {
                Ok(None)
            } else {
                Err(failed("stats", &out))
            };
        }
        let reading = stats::parse_stats(&out.stdout)?;
        // A container that has stopped still answers, with all zeros; a running
        // game always has at least one process.
        Ok((reading.pids > 0).then_some(reading))
    }

    async fn list_managed(&self) -> Result<Vec<ManagedPod>, RuntimeError> {
        let args: Vec<String> = [
            "pod",
            "ps",
            "--filter",
            &format!("label={LABEL_ID}"),
            "--format",
            "json",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let out = self.run_ok("pod ps", &args, self.timeouts.query).await?;
        let text = out.stdout.trim();
        if text.is_empty() || text == "null" {
            return Ok(Vec::new());
        }
        let pods: Vec<PodPs> = parse_json(text)?;
        Ok(pods
            .into_iter()
            .filter_map(|p| {
                let server_id = p.labels.as_ref()?.get(LABEL_ID)?.clone();
                Some(ManagedPod {
                    server_id,
                    pod_name: p.name,
                    status: p.status,
                    containers: p
                        .containers
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| c.names)
                        .collect(),
                })
            })
            .collect())
    }

    async fn version(&self) -> Result<String, RuntimeError> {
        let out = self
            .run_ok(
                "version",
                &[
                    "version".to_owned(),
                    "--format".into(),
                    "{{.Client.Version}}".into(),
                ],
                self.timeouts.query,
            )
            .await?;
        Ok(out.stdout.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn podman_warnings_are_not_game_output() {
        assert!(is_podman_noise(
            "time=\"2026-11-01T22:55:34+01:00\" level=warning msg=\"Falling back to --cgroup-manager=cgroupfs\""
        ));
        assert!(!is_podman_noise(
            "[21:55:33] [Server thread/INFO]: Done (1.8s)!"
        ));
        assert!(!is_podman_noise("time=noon and level=warning in chat"));
    }

    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn capped_line_reader() -> Result<(), Box<dyn std::error::Error>> {
        let (mut w, r) = tokio::io::duplex(64 * 1024);
        let long = "x".repeat(50_000);
        w.write_all(format!("short\r\n{long}\nlast-no-newline").as_bytes())
            .await?;
        drop(w);
        let mut r = BufReader::new(r);
        assert_eq!(
            read_capped_line(&mut r, 100).await?.as_deref(),
            Some("short")
        );
        let l2 = read_capped_line(&mut r, 100).await?.unwrap_or_default();
        assert!(l2.starts_with(&"x".repeat(100)));
        assert!(l2.ends_with("[line truncated]"));
        assert!(l2.len() < 200);
        assert_eq!(
            read_capped_line(&mut r, 100).await?.as_deref(),
            Some("last-no-newline")
        );
        assert_eq!(read_capped_line(&mut r, 100).await?, None);
        Ok(())
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "é".repeat(50);
        let t = truncate(&s, 11);
        assert!(t.ends_with("[truncated]"));
        assert_eq!(truncate("  ok \n", 100), "ok");
    }

    #[test]
    fn parses_inspect_state() -> Result<(), RuntimeError> {
        let json = r#"[{"State":{"Status":"exited","ExitCode":137,"OOMKilled":true,
            "StartedAt":"2026-11-01T10:00:00Z","Pid":0},"Other":1}]"#;
        let infos: Vec<ContainerInspect> = parse_json(json)?;
        let info = infos.first().map(|c| container_info(&c.state));
        assert_eq!(
            info,
            Some(ContainerInfo {
                state: ContainerState::Exited { code: 137 },
                oom_killed: true,
                started_at: Some("2026-11-01T10:00:00Z".into()),
            })
        );
        let json = r#"[{"State":{"Status":"created","StartedAt":"0001-01-01T00:00:00Z"}}]"#;
        let infos: Vec<ContainerInspect> = parse_json(json)?;
        let info = infos.first().map(|c| container_info(&c.state));
        assert_eq!(
            info.map(|i| (i.state, i.started_at)),
            Some((ContainerState::Other("created".into()), None))
        );
        Ok(())
    }

    #[test]
    fn parses_pod_inspect_array_and_object() -> Result<(), RuntimeError> {
        for json in [
            r#"[{"InfraContainerID":"abc","Other":{}}]"#,
            r#"{"InfraContainerID":"abc"}"#,
        ] {
            let v: OneOrMany<PodInspect> = parse_json(json)?;
            let id = match v {
                OneOrMany::One(p) => p.infra_container_id,
                OneOrMany::Many(v) => v
                    .into_iter()
                    .next()
                    .map(|p| p.infra_container_id)
                    .unwrap_or_default(),
            };
            assert_eq!(id, "abc");
        }
        Ok(())
    }

    #[test]
    fn parses_pod_ps() -> Result<(), RuntimeError> {
        let json = r#"[{"Id":"a","Name":"craft-x","Status":"Running",
            "Labels":{"craft-a-server.id":"x"},
            "Containers":[{"Id":"1","Names":"a-infra","Status":"running"},
                          {"Id":"2","Names":"craft-x-mc","Status":"running"}]},
            {"Name":"other","Status":"Running","Labels":null,"Containers":null}]"#;
        let pods: Vec<PodPs> = parse_json(json)?;
        assert_eq!(pods.len(), 2);
        assert_eq!(
            pods.first()
                .and_then(|p| p.containers.as_ref())
                .map(Vec::len),
            Some(2)
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_binary_is_a_spawn_error() {
        let cli = PodmanCli::new("definitely-not-a-real-podman-binary");
        let r = cli.version().await;
        assert!(matches!(r, Err(RuntimeError::Spawn { .. })), "{r:?}");
    }

    #[tokio::test]
    async fn invalid_inputs_are_rejected_before_spawning() {
        let cli = PodmanCli::new("definitely-not-a-real-podman-binary");
        assert!(matches!(
            cli.remove("Bad Id").await,
            Err(RuntimeError::InvalidSpec(_))
        ));
        assert!(matches!(
            cli.rcon("ok", "say hi\nop me").await,
            Err(RuntimeError::InvalidCommand(_))
        ));
    }
}
