//! Server model and its lifecycle state machine.
//!
//! The transition function is pure: it performs no I/O, so every rule is
//! unit-tested. Only the daemon applies transitions and persists the result.

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Lifecycle state of a managed server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum ServerState {
    Creating,
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed(String),
}

/// Something that happened to a server and may change its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    /// Data directory and container definition are ready.
    Created,
    /// A start was requested.
    StartRequested,
    /// The server answered the status ping.
    PingOk,
    /// A stop was requested.
    StopRequested,
    /// The container exited cleanly after a stop.
    Exited,
    /// Something went wrong; carries a user-facing reason.
    Failure(String),
    /// A failed server was acknowledged and reset by the user.
    Reset,
}

impl ServerState {
    /// Splits the state into a stable name and the failure reason, for storage.
    pub fn to_parts(&self) -> (&'static str, Option<&str>) {
        match self {
            ServerState::Creating => ("creating", None),
            ServerState::Stopped => ("stopped", None),
            ServerState::Starting => ("starting", None),
            ServerState::Running => ("running", None),
            ServerState::Stopping => ("stopping", None),
            ServerState::Failed(reason) => ("failed", Some(reason.as_str())),
        }
    }

    /// Inverse of [`ServerState::to_parts`]; `None` for an unknown name.
    pub fn from_parts(name: &str, reason: Option<&str>) -> Option<ServerState> {
        Some(match name {
            "creating" => ServerState::Creating,
            "stopped" => ServerState::Stopped,
            "starting" => ServerState::Starting,
            "running" => ServerState::Running,
            "stopping" => ServerState::Stopping,
            "failed" => ServerState::Failed(reason.unwrap_or("unknown").to_owned()),
            _ => return None,
        })
    }

    /// Returns the state after `event`, or an error if the move is illegal.
    pub fn next(&self, event: ServerEvent) -> Result<ServerState, Error> {
        use ServerEvent as E;
        use ServerState as S;
        match (self, &event) {
            (S::Creating, E::Created) => Ok(S::Stopped),
            (S::Stopped, E::StartRequested) => Ok(S::Starting),
            (S::Starting, E::PingOk) => Ok(S::Running),
            (S::Starting | S::Running, E::StopRequested) => Ok(S::Stopping),
            (S::Stopping, E::Exited) => Ok(S::Stopped),
            // A stop request while already stopping or stopped is idempotent.
            (S::Stopping, E::StopRequested) => Ok(S::Stopping),
            (S::Stopped, E::StopRequested) => Ok(S::Stopped),
            (S::Creating | S::Starting | S::Running | S::Stopping, E::Failure(reason)) => {
                Ok(S::Failed(reason.clone()))
            }
            (S::Running, E::Exited) => Ok(S::Failed("server exited unexpectedly".into())),
            (S::Failed(_), E::Reset) => Ok(S::Stopped),
            _ => Err(Error::InvalidTransition {
                from: self.clone(),
                event,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_roundtrip() {
        for s in [
            ServerState::Creating,
            ServerState::Stopped,
            ServerState::Starting,
            ServerState::Running,
            ServerState::Stopping,
            ServerState::Failed("boom".into()),
        ] {
            let (name, reason) = s.to_parts();
            assert_eq!(ServerState::from_parts(name, reason), Some(s.clone()));
        }
        assert_eq!(ServerState::from_parts("nope", None), None);
    }

    #[test]
    fn happy_path() -> Result<(), Error> {
        let s = ServerState::Creating
            .next(ServerEvent::Created)?
            .next(ServerEvent::StartRequested)?
            .next(ServerEvent::PingOk)?
            .next(ServerEvent::StopRequested)?
            .next(ServerEvent::Exited)?;
        assert_eq!(s, ServerState::Stopped);
        Ok(())
    }

    #[test]
    fn failure_carries_reason() -> Result<(), Error> {
        let s = ServerState::Starting.next(ServerEvent::Failure("boom".into()))?;
        assert_eq!(s, ServerState::Failed("boom".into()));
        Ok(())
    }

    #[test]
    fn stop_is_idempotent() -> Result<(), Error> {
        assert_eq!(
            ServerState::Stopped.next(ServerEvent::StopRequested)?,
            ServerState::Stopped
        );
        assert_eq!(
            ServerState::Stopping.next(ServerEvent::StopRequested)?,
            ServerState::Stopping
        );
        Ok(())
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert!(ServerState::Stopped.next(ServerEvent::PingOk).is_err());
        assert!(
            ServerState::Running
                .next(ServerEvent::StartRequested)
                .is_err()
        );
        assert!(
            ServerState::Failed("x".into())
                .next(ServerEvent::StartRequested)
                .is_err()
        );
    }

    #[test]
    fn failed_requires_reset() -> Result<(), Error> {
        let s = ServerState::Failed("x".into()).next(ServerEvent::Reset)?;
        assert_eq!(s, ServerState::Stopped);
        Ok(())
    }
}
