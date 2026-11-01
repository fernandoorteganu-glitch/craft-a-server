//! Container runtime wrapper (Podman CLI) and hardening flags. (Phase 1)
//!
//! Layout:
//! - [`spec`]: [`ServerContainerSpec`], input validation and the single pure
//!   function that builds the hardened `podman run` argument list.
//! - [`egress`]: the nftables egress policy applied into each pod's netns.
//! - [`podman`]: [`PodmanCli`], the real implementation (argument vectors only).
//! - [`fake`]: [`FakeRuntime`], an in-memory implementation for lifecycle tests.
//! - [`status`]: [`mc_status`], a real Server List Ping client.

use std::future::Future;
use std::time::Duration;

use tokio::sync::mpsc;

pub mod egress;
pub mod fake;
pub mod podman;
pub mod spec;
pub mod status;

pub use egress::EgressPolicy;
pub use fake::{FakeRuntime, RuntimeCall, RuntimeOp};
pub use podman::PodmanCli;
pub use spec::{ServerContainerSpec, build_pod_create_args, build_run_args};
pub use status::{StatusError, mc_status};

/// Label carrying the server id on pods and containers (used for reconciliation).
pub const LABEL_ID: &str = "craft-a-server.id";
/// Port the Minecraft server listens on inside the pod.
pub const MC_PORT: u16 = 25565;

/// Name of the pod for a server id.
pub fn pod_name(id: &str) -> String {
    format!("craft-{id}")
}

/// Name of the game container for a server id.
pub fn container_name(id: &str) -> String {
    format!("craft-{id}-mc")
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum RuntimeError {
    #[error("invalid server spec: {0}")]
    InvalidSpec(String),
    #[error("the Minecraft EULA has not been accepted")]
    EulaNotAccepted,
    #[error("invalid RCON command: {0}")]
    InvalidCommand(String),
    #[error("could not run {op}: {message}")]
    Spawn { op: String, message: String },
    #[error("{op} timed out after {secs}s")]
    Timeout { op: String, secs: u64 },
    #[error("{op} failed (exit {code:?}): {stderr}")]
    CommandFailed {
        op: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("could not parse podman output: {0}")]
    Parse(String),
    #[error("egress rules could not be applied or verified (pod removed): {0}")]
    Egress(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0} already exists")]
    AlreadyExists(String),
}

/// State of the game container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    Exited {
        code: i32,
    },
    /// The container does not exist (never created, or removed).
    Missing,
    /// Created, paused, stopping... anything else, with podman's own word for it.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub state: ContainerState,
    pub oom_killed: bool,
    /// RFC 3339 start time as reported by podman, when the container has started.
    pub started_at: Option<String>,
}

impl ContainerInfo {
    pub fn missing() -> Self {
        Self {
            state: ContainerState::Missing,
            oom_killed: false,
            started_at: None,
        }
    }
}

/// A pod carrying our label, found at boot reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPod {
    pub server_id: String,
    pub pod_name: String,
    /// Podman's pod status word (`Running`, `Exited`, `Degraded`, ...).
    pub status: String,
    /// Names of the containers in the pod (infra included).
    pub containers: Vec<String>,
}

/// Bounded channel of log lines; closes when the container's log ends.
pub type LogReceiver = mpsc::Receiver<String>;

/// Everything the lifecycle service needs from a container engine.
///
/// Futures are `Send` so implementations can be driven from spawned tasks.
pub trait ContainerRuntime: Send + Sync {
    /// Pod + egress rules + game container. Fails closed: the game container
    /// never starts unless the egress table is verified in the pod netns. On any
    /// failure the pod is removed. Errors with `AlreadyExists` if the pod exists
    /// (call `remove` first).
    fn create_and_start(
        &self,
        spec: &ServerContainerSpec,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Graceful stop of the game container (SIGTERM, then SIGKILL after `grace`).
    fn stop(
        &self,
        id: &str,
        grace: Duration,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// SIGKILL the game container.
    fn kill(&self, id: &str) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Removes the pod and every container in it. Idempotent.
    fn remove(&self, id: &str) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    fn inspect(&self, id: &str)
    -> impl Future<Output = Result<ContainerInfo, RuntimeError>> + Send;

    /// Follows the log from now on (use `logs_tail` for history). Lines are
    /// capped in length; dropping the receiver ends the follow.
    fn logs_follow(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<LogReceiver, RuntimeError>> + Send;

    fn logs_tail(
        &self,
        id: &str,
        lines: usize,
    ) -> impl Future<Output = Result<Vec<String>, RuntimeError>> + Send;

    /// Runs one RCON command through `rcon-cli` inside the container.
    fn rcon(
        &self,
        id: &str,
        command: &str,
    ) -> impl Future<Output = Result<String, RuntimeError>> + Send;

    /// All pods carrying the `craft-a-server.id` label.
    fn list_managed(&self) -> impl Future<Output = Result<Vec<ManagedPod>, RuntimeError>> + Send;

    fn version(&self) -> impl Future<Output = Result<String, RuntimeError>> + Send;
}

/// Splits an RCON command into argv words. Rejects empty commands, control
/// characters and over-long input.
pub(crate) fn rcon_words(command: &str) -> Result<Vec<String>, RuntimeError> {
    if command.len() > 1024 {
        return Err(RuntimeError::InvalidCommand("command too long".into()));
    }
    if command.chars().any(char::is_control) {
        return Err(RuntimeError::InvalidCommand(
            "control characters (including newlines) are not allowed".into(),
        ));
    }
    let words: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
    if words.is_empty() {
        return Err(RuntimeError::InvalidCommand("empty command".into()));
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rcon_words_splits_and_rejects() -> Result<(), RuntimeError> {
        assert_eq!(rcon_words("save-all  flush")?, vec!["save-all", "flush"]);
        assert!(rcon_words("").is_err());
        assert!(rcon_words("   ").is_err());
        assert!(rcon_words("say hi\nop attacker").is_err());
        assert!(rcon_words("say \0").is_err());
        assert!(rcon_words(&"a".repeat(2000)).is_err());
        Ok(())
    }

    #[test]
    fn names_are_derived_from_id() {
        assert_eq!(pod_name("abc-1"), "craft-abc-1");
        assert_eq!(container_name("abc-1"), "craft-abc-1-mc");
    }
}
