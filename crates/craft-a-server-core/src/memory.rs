//! Memory sizing for a server container.
//!
//! Rule from the spec: the game gets total RAM minus a reserve for the OS and
//! the daemon, up to a configurable cap, and the panel warns when little is
//! left. Pure so it is trivially testable; the daemon feeds it `/proc/meminfo`.

const MIB: u64 = 1024 * 1024;
/// Reserve for the OS, the daemon and the playit agent.
pub const DEFAULT_RESERVE_MIB: u64 = 1536;
/// Below this the panel warns that the server will be cramped.
pub const WARN_BELOW_MIB: u64 = 2048;
/// JVM heap never goes below this; under it we refuse to start.
pub const MIN_HEAP_MIB: u64 = 512;
/// Extra room the container limit gets over the JVM heap (metaspace, threads, GC).
pub const NON_HEAP_OVERHEAD_MIB: u64 = 768;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPlan {
    /// Value for the JVM `-Xmx` (MiB).
    pub heap_mib: u64,
    /// Hard container memory limit (MiB), heap plus overhead.
    pub container_limit_mib: u64,
    /// True when the heap is under [`WARN_BELOW_MIB`].
    pub low_memory_warning: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryError {
    #[error(
        "not enough memory: {available_mib} MiB left for the game, at least {MIN_HEAP_MIB} MiB are required"
    )]
    TooLittle { available_mib: u64 },
}

/// Computes the plan for a machine with `total_ram_bytes`.
///
/// `user_max_mib` is the optional cap chosen by the user ("advanced").
pub fn plan(
    total_ram_bytes: u64,
    reserve_mib: u64,
    user_max_mib: Option<u64>,
) -> Result<MemoryPlan, MemoryError> {
    let total_mib = total_ram_bytes / MIB;
    let available = total_mib.saturating_sub(reserve_mib);
    let capped = user_max_mib.map_or(available, |m| m.min(available));
    // The container limit includes overhead, so carve it out of the budget.
    let heap = capped.saturating_sub(NON_HEAP_OVERHEAD_MIB);
    if heap < MIN_HEAP_MIB {
        return Err(MemoryError::TooLittle {
            available_mib: available,
        });
    }
    Ok(MemoryPlan {
        heap_mib: heap,
        container_limit_mib: heap + NON_HEAP_OVERHEAD_MIB,
        low_memory_warning: heap < WARN_BELOW_MIB,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;

    #[test]
    fn eight_gib_machine() -> Result<(), MemoryError> {
        let p = plan(8 * GIB, DEFAULT_RESERVE_MIB, None)?;
        // 8192 - 1536 = 6656 budget; heap = 6656 - 768
        assert_eq!(p.heap_mib, 5888);
        assert_eq!(p.container_limit_mib, 6656);
        assert!(!p.low_memory_warning);
        Ok(())
    }

    #[test]
    fn four_gib_machine_warns() -> Result<(), MemoryError> {
        let p = plan(4 * GIB, DEFAULT_RESERVE_MIB, None)?;
        assert_eq!(p.heap_mib, 4096 - 1536 - 768);
        assert!(p.low_memory_warning);
        Ok(())
    }

    #[test]
    fn user_cap_is_respected_and_never_exceeds_budget() -> Result<(), MemoryError> {
        let p = plan(16 * GIB, DEFAULT_RESERVE_MIB, Some(4096))?;
        assert_eq!(p.container_limit_mib, 4096);
        let p = plan(4 * GIB, DEFAULT_RESERVE_MIB, Some(64 * 1024))?;
        assert!(p.container_limit_mib <= 4096 - 1536);
        Ok(())
    }

    #[test]
    fn tiny_machine_is_rejected() {
        assert!(matches!(
            plan(2 * GIB, DEFAULT_RESERVE_MIB, None),
            Err(MemoryError::TooLittle { .. })
        ));
        assert!(plan(0, DEFAULT_RESERVE_MIB, None).is_err());
    }
}
