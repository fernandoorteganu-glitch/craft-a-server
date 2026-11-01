//! Domain errors shared across crates.

use crate::model::ServerState;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid server state transition from {from:?} on {event:?}")]
    InvalidTransition {
        from: ServerState,
        event: crate::model::ServerEvent,
    },
}
