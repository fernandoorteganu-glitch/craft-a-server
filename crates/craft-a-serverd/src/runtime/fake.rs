//! [`FakeRuntime`]: an in-memory [`ContainerRuntime`] for lifecycle tests. It
//! records every call, lets tests script failures, and simulates container state.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;

use super::spec::{ServerContainerSpec, build_pod_create_args, build_run_args, validate_id};
use super::{
    ContainerInfo, ContainerRuntime, ContainerState, LogReceiver, ManagedPod, RuntimeError,
    container_name, pod_name, rcon_words,
};

/// Operation names, used to script failures and to describe recorded calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeOp {
    CreateAndStart,
    Stop,
    Kill,
    Remove,
    Inspect,
    LogsFollow,
    LogsTail,
    Rcon,
    ListManaged,
    Version,
}

/// One recorded call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCall {
    CreateAndStart { id: String },
    Stop { id: String, grace: Duration },
    Kill { id: String },
    Remove { id: String },
    Inspect { id: String },
    LogsFollow { id: String },
    LogsTail { id: String, lines: usize },
    Rcon { id: String, command: String },
    ListManaged,
    Version,
}

#[derive(Debug)]
struct FakeServer {
    spec: ServerContainerSpec,
    state: ContainerState,
    oom_killed: bool,
    log: Vec<String>,
    followers: Vec<mpsc::Sender<String>>,
}

#[derive(Debug, Default)]
struct Inner {
    servers: HashMap<String, FakeServer>,
    calls: Vec<RuntimeCall>,
    failures: HashMap<RuntimeOp, VecDeque<RuntimeError>>,
    rcon_replies: HashMap<String, String>,
}

/// Cloneable handle; clones share state.
#[derive(Debug, Clone, Default)]
pub struct FakeRuntime {
    inner: Arc<Mutex<Inner>>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next call of `op` fails with `err` (queued FIFO; consumed per call).
    pub fn fail_next(&self, op: RuntimeOp, err: RuntimeError) {
        self.lock().failures.entry(op).or_default().push_back(err);
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<RuntimeCall> {
        self.lock().calls.clone()
    }

    /// Simulates the game process exiting on its own (crash, OOM, clean exit).
    pub fn simulate_exit(&self, id: &str, code: i32, oom_killed: bool) {
        if let Some(s) = self.lock().servers.get_mut(id) {
            s.state = ContainerState::Exited { code };
            s.oom_killed = oom_killed;
        }
    }

    /// Simulates the container disappearing behind the daemon's back (pod kept).
    pub fn simulate_vanish(&self, id: &str) {
        if let Some(s) = self.lock().servers.get_mut(id) {
            s.state = ContainerState::Missing;
        }
    }

    /// Appends a log line, delivering it to followers.
    pub fn push_log(&self, id: &str, line: &str) {
        if let Some(s) = self.lock().servers.get_mut(id) {
            s.log.push(line.to_owned());
            s.followers
                .retain(|tx| tx.try_send(line.to_owned()).is_ok());
        }
    }

    /// Fixes the reply `rcon` returns for an exact command string.
    pub fn set_rcon_reply(&self, command: &str, reply: &str) {
        self.lock()
            .rcon_replies
            .insert(command.to_owned(), reply.to_owned());
    }

    /// Creates a pod as if left over from an earlier daemon run (for
    /// reconciliation tests).
    pub fn seed_pod(&self, spec: ServerContainerSpec, state: ContainerState) {
        self.lock().servers.insert(
            spec.id.clone(),
            FakeServer {
                spec,
                state,
                oom_killed: false,
                log: Vec::new(),
                followers: Vec::new(),
            },
        );
    }

    /// Specs currently held (pods that exist), keyed by server id.
    pub fn pod_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().servers.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Records the call and pops a scripted failure for it, if any.
    fn begin(&self, call: RuntimeCall, op: RuntimeOp) -> Result<(), RuntimeError> {
        let mut g = self.lock();
        g.calls.push(call);
        match g.failures.get_mut(&op).and_then(VecDeque::pop_front) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl ContainerRuntime for FakeRuntime {
    async fn create_and_start(&self, spec: &ServerContainerSpec) -> Result<(), RuntimeError> {
        self.begin(
            RuntimeCall::CreateAndStart {
                id: spec.id.clone(),
            },
            RuntimeOp::CreateAndStart,
        )?;
        // Same validation as the real runtime, before any state change.
        build_pod_create_args(spec)?;
        build_run_args(spec)?;
        let mut g = self.lock();
        if g.servers.contains_key(&spec.id) {
            return Err(RuntimeError::AlreadyExists(format!(
                "pod {}",
                pod_name(&spec.id)
            )));
        }
        g.servers.insert(
            spec.id.clone(),
            FakeServer {
                spec: spec.clone(),
                state: ContainerState::Running,
                oom_killed: false,
                log: Vec::new(),
                followers: Vec::new(),
            },
        );
        Ok(())
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<(), RuntimeError> {
        self.begin(
            RuntimeCall::Stop {
                id: id.to_owned(),
                grace,
            },
            RuntimeOp::Stop,
        )?;
        validate_id(id)?;
        let mut g = self.lock();
        match g.servers.get_mut(id) {
            Some(s) if s.state != ContainerState::Missing => {
                if s.state == ContainerState::Running {
                    s.state = ContainerState::Exited { code: 0 };
                }
                Ok(())
            }
            _ => Err(RuntimeError::NotFound(format!(
                "container {}",
                container_name(id)
            ))),
        }
    }

    async fn kill(&self, id: &str) -> Result<(), RuntimeError> {
        self.begin(RuntimeCall::Kill { id: id.to_owned() }, RuntimeOp::Kill)?;
        validate_id(id)?;
        let mut g = self.lock();
        match g.servers.get_mut(id) {
            Some(s) if s.state != ContainerState::Missing => {
                if s.state == ContainerState::Running {
                    s.state = ContainerState::Exited { code: 137 };
                }
                Ok(())
            }
            _ => Err(RuntimeError::NotFound(format!(
                "container {}",
                container_name(id)
            ))),
        }
    }

    async fn remove(&self, id: &str) -> Result<(), RuntimeError> {
        self.begin(RuntimeCall::Remove { id: id.to_owned() }, RuntimeOp::Remove)?;
        validate_id(id)?;
        self.lock().servers.remove(id);
        Ok(())
    }

    async fn inspect(&self, id: &str) -> Result<ContainerInfo, RuntimeError> {
        self.begin(
            RuntimeCall::Inspect { id: id.to_owned() },
            RuntimeOp::Inspect,
        )?;
        validate_id(id)?;
        let g = self.lock();
        Ok(g.servers
            .get(id)
            .map_or_else(ContainerInfo::missing, |s| ContainerInfo {
                started_at: (s.state != ContainerState::Missing)
                    .then(|| "2026-01-01T00:00:00Z".to_owned()),
                state: s.state.clone(),
                oom_killed: s.oom_killed,
            }))
    }

    async fn logs_follow(&self, id: &str) -> Result<LogReceiver, RuntimeError> {
        self.begin(
            RuntimeCall::LogsFollow { id: id.to_owned() },
            RuntimeOp::LogsFollow,
        )?;
        validate_id(id)?;
        let mut g = self.lock();
        let s = g
            .servers
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound(format!("container {}", container_name(id))))?;
        let (tx, rx) = mpsc::channel(1024);
        s.followers.push(tx);
        Ok(rx)
    }

    async fn logs_tail(&self, id: &str, lines: usize) -> Result<Vec<String>, RuntimeError> {
        self.begin(
            RuntimeCall::LogsTail {
                id: id.to_owned(),
                lines,
            },
            RuntimeOp::LogsTail,
        )?;
        validate_id(id)?;
        let g = self.lock();
        let s = g
            .servers
            .get(id)
            .ok_or_else(|| RuntimeError::NotFound(format!("container {}", container_name(id))))?;
        let skip = s.log.len().saturating_sub(lines);
        Ok(s.log.iter().skip(skip).cloned().collect())
    }

    async fn rcon(&self, id: &str, command: &str) -> Result<String, RuntimeError> {
        self.begin(
            RuntimeCall::Rcon {
                id: id.to_owned(),
                command: command.to_owned(),
            },
            RuntimeOp::Rcon,
        )?;
        let words = rcon_words(command)?;
        validate_id(id)?;
        let g = self.lock();
        match g.servers.get(id) {
            Some(s) if s.state == ContainerState::Running => {
                Ok(g.rcon_replies.get(command).cloned().unwrap_or_else(|| {
                    // Mirrors rcon-cli's habit of echoing something for most commands.
                    format!("ok: {}", words.join(" "))
                }))
            }
            _ => Err(RuntimeError::NotFound(format!(
                "running container {}",
                container_name(id)
            ))),
        }
    }

    async fn list_managed(&self) -> Result<Vec<ManagedPod>, RuntimeError> {
        self.begin(RuntimeCall::ListManaged, RuntimeOp::ListManaged)?;
        let g = self.lock();
        let mut pods: Vec<ManagedPod> = g
            .servers
            .values()
            .map(|s| {
                let mut containers = vec![format!("{}-infra", pod_name(&s.spec.id))];
                if s.state != ContainerState::Missing {
                    containers.push(container_name(&s.spec.id));
                }
                ManagedPod {
                    server_id: s.spec.id.clone(),
                    pod_name: pod_name(&s.spec.id),
                    status: match s.state {
                        ContainerState::Running => "Running",
                        _ => "Degraded",
                    }
                    .to_owned(),
                    containers,
                }
            })
            .collect();
        pods.sort_by(|a, b| a.server_id.cmp(&b.server_id));
        Ok(pods)
    }

    async fn version(&self) -> Result<String, RuntimeError> {
        self.begin(RuntimeCall::Version, RuntimeOp::Version)?;
        Ok("5.4.2-fake".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::spec::test_spec;

    #[tokio::test]
    async fn lifecycle_and_recording() -> Result<(), RuntimeError> {
        let rt = FakeRuntime::new();
        let spec = test_spec();
        assert_eq!(rt.inspect(&spec.id).await?.state, ContainerState::Missing);
        rt.create_and_start(&spec).await?;
        assert!(matches!(
            rt.create_and_start(&spec).await,
            Err(RuntimeError::AlreadyExists(_))
        ));
        assert_eq!(rt.inspect(&spec.id).await?.state, ContainerState::Running);
        assert_eq!(rt.list_managed().await?.len(), 1);

        rt.simulate_exit(&spec.id, 137, true);
        let info = rt.inspect(&spec.id).await?;
        assert_eq!(info.state, ContainerState::Exited { code: 137 });
        assert!(info.oom_killed);

        rt.remove(&spec.id).await?;
        rt.remove(&spec.id).await?; // idempotent
        assert!(rt.list_managed().await?.is_empty());
        assert!(matches!(
            rt.stop(&spec.id, Duration::from_secs(1)).await,
            Err(RuntimeError::NotFound(_))
        ));
        assert!(rt.calls().contains(&RuntimeCall::Remove {
            id: spec.id.clone()
        }));
        Ok(())
    }

    #[tokio::test]
    async fn scripted_failures_are_consumed_once() -> Result<(), RuntimeError> {
        let rt = FakeRuntime::new();
        let spec = test_spec();
        rt.fail_next(
            RuntimeOp::CreateAndStart,
            RuntimeError::Egress("boom".into()),
        );
        assert!(matches!(
            rt.create_and_start(&spec).await,
            Err(RuntimeError::Egress(_))
        ));
        assert!(rt.pod_ids().is_empty());
        rt.create_and_start(&spec).await?;
        Ok(())
    }

    #[tokio::test]
    async fn rejects_eula_and_bad_commands() -> Result<(), RuntimeError> {
        let rt = FakeRuntime::new();
        let mut spec = test_spec();
        spec.eula_accepted = false;
        assert!(matches!(
            rt.create_and_start(&spec).await,
            Err(RuntimeError::EulaNotAccepted)
        ));
        spec.eula_accepted = true;
        rt.create_and_start(&spec).await?;
        assert!(rt.rcon(&spec.id, "say a\nb").await.is_err());
        rt.set_rcon_reply("list", "There are 0 of a max of 20 players online:");
        assert_eq!(
            rt.rcon(&spec.id, "list").await?,
            "There are 0 of a max of 20 players online:"
        );
        Ok(())
    }

    #[tokio::test]
    async fn logs_follow_and_tail() -> Result<(), RuntimeError> {
        let rt = FakeRuntime::new();
        let spec = test_spec();
        rt.create_and_start(&spec).await?;
        let mut rx = rt.logs_follow(&spec.id).await?;
        rt.push_log(&spec.id, "one");
        rt.push_log(&spec.id, "two");
        assert_eq!(rx.recv().await.as_deref(), Some("one"));
        assert_eq!(rx.recv().await.as_deref(), Some("two"));
        assert_eq!(rt.logs_tail(&spec.id, 1).await?, vec!["two".to_owned()]);
        Ok(())
    }
}
