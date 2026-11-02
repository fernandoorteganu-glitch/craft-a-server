//! The playit.gg tunnel's persisted state (schema v5): one row.

use craft_a_server_core::tunnel::TunnelState;
use rusqlite::params;

use super::{Db, Result};

/// What is stored about the tunnel. The playit secret is never in the
/// database: it lives in a 0600 file next to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelRow {
    /// The user turned online play on (and confirmed the disclosure).
    pub enabled: bool,
    pub state: TunnelState,
    /// When the disclosure was confirmed; kept after disabling as a record.
    pub consent_unix: Option<i64>,
    pub updated_unix: i64,
}

/// The columns of the `tunnel` row as stored.
type RawRow = (
    bool,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    i64,
);

impl Db {
    /// The tunnel row. Stored values that do not pass validation (a state
    /// name this build does not know, an address that is not a hostname) read
    /// back as a disabled tunnel in `Error`: the safe direction.
    pub fn get_tunnel(&self) -> Result<TunnelRow> {
        let conn = self.lock()?;
        let (enabled, state, claim_url, address, reason, consent_unix, updated_unix): RawRow = conn
            .query_row(
                "SELECT enabled, state, claim_url, address, reason, consent_unix, updated_unix
             FROM tunnel WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?;
        Ok(
            match TunnelState::from_parts(
                &state,
                claim_url.as_deref(),
                address.as_deref(),
                reason.as_deref(),
            ) {
                Some(state) => TunnelRow {
                    enabled,
                    state,
                    consent_unix,
                    updated_unix,
                },
                None => TunnelRow {
                    enabled: false,
                    state: TunnelState::Error {
                        reason: "the stored tunnel state was not valid and was ignored".into(),
                    },
                    consent_unix,
                    updated_unix,
                },
            },
        )
    }

    pub fn set_tunnel(&self, row: &TunnelRow) -> Result<()> {
        let (claim_url, address, reason) = match &row.state {
            TunnelState::Claiming { url, .. } => (Some(url.as_str()), None, None),
            TunnelState::Running { address } => (None, Some(address.as_str()), None),
            TunnelState::Error { reason } => (None, None, Some(reason.as_str())),
            TunnelState::NotConfigured | TunnelState::Claimed => (None, None, None),
        };
        self.lock()?.execute(
            "UPDATE tunnel SET enabled = ?1, state = ?2, claim_url = ?3, address = ?4,
                 reason = ?5, consent_unix = ?6, updated_unix = ?7
             WHERE id = 1",
            params![
                row.enabled,
                row.state.name(),
                claim_url,
                address,
                reason,
                row.consent_unix,
                row.updated_unix
            ],
        )?;
        Ok(())
    }
}
