//! The nftables egress policy applied into every server pod's network namespace
//! (see `docs/spikes/network-containment.md`).

use std::path::Path;

use super::RuntimeError;

/// Name of the table the policy creates; used to verify it is active.
pub const EGRESS_TABLE: &str = "craft_egress";

/// Default ruleset, embedded at build time from `deploy/podman/egress.nft`.
pub const DEFAULT_EGRESS_RULESET: &str = include_str!("../../../../deploy/podman/egress.nft");

/// The ruleset to apply. Fed to `nft -f -` on stdin, so no temp file is needed.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    ruleset: String,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            ruleset: DEFAULT_EGRESS_RULESET.to_owned(),
        }
    }
}

impl EgressPolicy {
    /// Uses a custom ruleset. It must define `table inet craft_egress`, otherwise
    /// the post-apply verification (and therefore every start) would fail closed.
    pub fn from_ruleset(ruleset: impl Into<String>) -> Result<Self, RuntimeError> {
        let ruleset = ruleset.into();
        if !ruleset.contains(&format!("table inet {EGRESS_TABLE}")) {
            return Err(RuntimeError::Egress(format!(
                "ruleset does not define table inet {EGRESS_TABLE}"
            )));
        }
        Ok(Self { ruleset })
    }

    /// Reads the ruleset from a file (for admins who tune the policy).
    pub fn from_file(path: &Path) -> Result<Self, RuntimeError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| RuntimeError::Egress(format!("reading {}: {e}", path.display())))?;
        Self::from_ruleset(text)
    }

    pub fn ruleset(&self) -> &str {
        &self.ruleset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_defines_the_verified_table() {
        assert!(DEFAULT_EGRESS_RULESET.contains("table inet craft_egress"));
        assert!(EgressPolicy::default().ruleset().contains("192.168.0.0/16"));
    }

    #[test]
    fn custom_ruleset_must_define_table() {
        assert!(EgressPolicy::from_ruleset("table inet other {}").is_err());
        assert!(EgressPolicy::from_ruleset("table inet craft_egress {}").is_ok());
    }
}
