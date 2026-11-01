//! Tunnel abstraction. The MVP has a single implementation (playit.gg) that
//! lives in `craft-a-serverd`; keeping the interface here makes it swappable.

use serde::{Deserialize, Serialize};

/// Observable state of the tunnel, shown in the panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TunnelStatus {
    /// The agent is running but the user has not claimed it yet.
    Unclaimed {
        claim_url: String,
    },
    Connecting,
    Connected {
        public_address: String,
    },
    Reconnecting,
    Failed {
        reason: String,
    },
    Stopped,
}
