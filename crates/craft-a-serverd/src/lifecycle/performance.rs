//! Performance: the view-distance setting and a snapshot of resource use.

use std::time::{Duration, Instant};

use craft_a_server_core::memory;
use craft_a_server_core::model::ServerState;

use super::{DiskUsage, Result, Service, ServiceError, StatusProbe, parse_meminfo_total};
use crate::runtime::ContainerRuntime;
use crate::settings::{self, VIEW_DISTANCE_MAX, VIEW_DISTANCE_MIN};

/// A CPU reading kept between two looks, to turn cumulative CPU time into a rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CpuSample {
    at: Instant,
    cpu_time: Duration,
    /// Cores in use between the last two readings, once there are two.
    cores: Option<f64>,
}

/// Readings further apart than this start a new baseline: an average over
/// minutes would say nothing about "now".
const CPU_WINDOW_MAX: Duration = Duration::from_secs(30);
/// Readings closer than this keep the previous rate (too short to be accurate).
const CPU_WINDOW_MIN: Duration = Duration::from_secs(1);

/// The next sample after a new CPU-time reading.
pub(super) fn next_cpu_sample(
    prev: Option<CpuSample>,
    now: Instant,
    cpu_time: Duration,
) -> CpuSample {
    let fresh = CpuSample {
        at: now,
        cpu_time,
        cores: None,
    };
    let Some(prev) = prev else { return fresh };
    // The container restarted: its counter started again from zero.
    if cpu_time < prev.cpu_time {
        return fresh;
    }
    let wall = now.saturating_duration_since(prev.at);
    if wall > CPU_WINDOW_MAX {
        return fresh;
    }
    if wall < CPU_WINDOW_MIN {
        return prev;
    }
    CpuSample {
        at: now,
        cpu_time,
        cores: Some((cpu_time - prev.cpu_time).as_secs_f64() / wall.as_secs_f64()),
    }
}

/// What the game container is using right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GameUse {
    pub mem_used_bytes: u64,
    /// Cores' worth of CPU in use; `None` until a second reading exists.
    pub cpu_cores: Option<f64>,
    pub pids: u32,
}

/// The whole machine, from `/proc`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostUse {
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,
    /// One-minute load average.
    pub load1: f64,
}

/// A snapshot for the performance screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resources {
    pub running: bool,
    /// `None` when the server is not running or podman gave no reading.
    pub game: Option<GameUse>,
    /// What the game is allowed: container memory limit and Java heap (bytes).
    pub memory_limit_bytes: Option<u64>,
    pub heap_bytes: Option<u64>,
    /// Cores the container may use, and its process limit.
    pub cpus: u32,
    pub pids_limit: u32,
    pub host: Option<HostUse>,
    pub disk: Option<DiskUsage>,
}

const MIB: u64 = 1024 * 1024;

impl<R: ContainerRuntime, P: StatusProbe> Service<R, P> {
    /// Cores the game container may use.
    pub(crate) fn effective_cpus(&self) -> u32 {
        if self.cfg.cpus == 0 {
            std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
        } else {
            self.cfg.cpus
        }
    }

    /// The view distance the user chose (`None` = never chosen: the game's own).
    pub fn view_distance(&self, id: &str) -> Result<Option<u8>> {
        self.get_row(id)?;
        Ok(settings::load(&self.server_dir(id)).view_distance)
    }

    /// Sets the view distance. It reaches the game the next time the server
    /// starts; a running server is not touched.
    ///
    /// There is no "clear": the game keeps what it last wrote to its own
    /// settings when the variable stops being sent, so going back to the
    /// default means sending the default value (see
    /// [`settings::VIEW_DISTANCE_DEFAULT`]).
    ///
    /// Takes no server lock on purpose: the file write is atomic, and a start
    /// or restart in progress would otherwise make the screen wait for minutes.
    pub fn set_view_distance(&self, id: &str, value: u8) -> Result<()> {
        if !settings::valid_view_distance(value) {
            return Err(ServiceError::InvalidInput(format!(
                "the view distance must be between {VIEW_DISTANCE_MIN} and {VIEW_DISTANCE_MAX}"
            )));
        }
        self.get_row(id)?;
        let dir = self.server_dir(id);
        let mut current = settings::load(&dir);
        current.view_distance = Some(value);
        settings::save(&dir, &current).map_err(ServiceError::io("saving the server settings"))?;
        self.event(Some(id), "settings", &format!("view distance {value}"));
        Ok(())
    }

    /// What the server and the machine are using right now.
    pub async fn resources(&self, id: &str) -> Result<Resources> {
        let row = self.get_row(id)?;
        let running = row.state == ServerState::Running;
        let plan = memory::plan(
            self.env.total_ram_bytes,
            self.cfg.memory_reserve_mib,
            row.memory_max_mib.or(self.cfg.memory_max_mib),
        )
        .ok();

        let game = if running {
            match self.rt.stats(id).await {
                Ok(Some(reading)) => Some(GameUse {
                    mem_used_bytes: reading.mem_used_bytes,
                    cpu_cores: self.cpu_cores(id, reading.cpu_time),
                    pids: reading.pids,
                }),
                Ok(None) => None,
                Err(e) => {
                    tracing::debug!(server = %id, error = %e, "no resource reading");
                    None
                }
            }
        } else {
            self.forget_cpu(id);
            None
        };

        Ok(Resources {
            running,
            game,
            memory_limit_bytes: plan
                .as_ref()
                .map(|p| p.container_limit_mib.saturating_mul(MIB)),
            heap_bytes: plan.as_ref().map(|p| p.heap_mib.saturating_mul(MIB)),
            cpus: self.effective_cpus(),
            pids_limit: self.cfg.pids_limit,
            host: read_host(self.env.total_ram_bytes),
            disk: (self.env.disk)(&self.cfg.servers_dir()).ok(),
        })
    }

    fn cpu_cores(&self, id: &str, cpu_time: Duration) -> Option<f64> {
        let mut map = self.cpu_samples.lock().unwrap_or_else(|e| e.into_inner());
        let next = next_cpu_sample(map.get(id).copied(), Instant::now(), cpu_time);
        map.insert(id.to_owned(), next);
        next.cores
    }

    fn forget_cpu(&self, id: &str) {
        let mut map = self.cpu_samples.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(id);
    }
}

/// `/proc/meminfo` and `/proc/loadavg`; `None` where they do not exist.
fn read_host(total_hint: u64) -> Option<HostUse> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let load = std::fs::read_to_string("/proc/loadavg").ok()?;
    Some(HostUse {
        mem_total_bytes: parse_meminfo_total(&meminfo).unwrap_or(total_hint),
        mem_available_bytes: parse_meminfo_kb(&meminfo, "MemAvailable:")?,
        load1: parse_loadavg(&load)?,
    })
}

/// One `/proc/meminfo` value (kB) in bytes.
pub(super) fn parse_meminfo_kb(text: &str, key: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with(key))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024)
}

/// The one-minute load average from `/proc/loadavg`.
pub(super) fn parse_loadavg(text: &str) -> Option<f64> {
    let v: f64 = text.split_whitespace().next()?.parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn the_first_reading_has_no_rate() {
        let t0 = Instant::now();
        let s = next_cpu_sample(None, t0, sec(10));
        assert_eq!(s.cores, None);
    }

    #[test]
    fn two_readings_give_cores_in_use() {
        let t0 = Instant::now();
        let a = next_cpu_sample(None, t0, sec(10));
        // 3 s of CPU in 3 s of wall time: one core.
        let b = next_cpu_sample(Some(a), t0 + sec(3), sec(13));
        assert_eq!(b.cores, Some(1.0));
        // 1.5 s of CPU in 3 s: half a core.
        let c = next_cpu_sample(Some(b), t0 + sec(6), Duration::from_millis(14_500));
        assert_eq!(c.cores, Some(0.5));
    }

    #[test]
    fn a_reading_too_soon_keeps_the_last_rate() {
        let t0 = Instant::now();
        let a = next_cpu_sample(None, t0, sec(0));
        let b = next_cpu_sample(Some(a), t0 + sec(3), sec(3));
        let c = next_cpu_sample(Some(b), t0 + sec(3) + Duration::from_millis(200), sec(4));
        assert_eq!(c, b);
    }

    #[test]
    fn a_restart_or_a_long_gap_starts_over() {
        let t0 = Instant::now();
        let a = next_cpu_sample(None, t0, sec(100));
        // The counter went backwards: a new container.
        assert_eq!(next_cpu_sample(Some(a), t0 + sec(3), sec(2)).cores, None);
        // Nobody looked for a minute: an average over that says nothing.
        assert_eq!(next_cpu_sample(Some(a), t0 + sec(60), sec(150)).cores, None);
    }

    #[test]
    fn proc_files_are_parsed_strictly() {
        let meminfo = "MemTotal:       8000000 kB\nMemAvailable:   2500000 kB\n";
        assert_eq!(
            parse_meminfo_kb(meminfo, "MemAvailable:"),
            Some(2_500_000 * 1024)
        );
        assert_eq!(parse_meminfo_kb(meminfo, "Nothing:"), None);
        assert_eq!(
            parse_meminfo_kb("MemAvailable: lots kB", "MemAvailable:"),
            None
        );
        assert_eq!(parse_loadavg("0.52 0.40 0.31 1/234 5678"), Some(0.52));
        assert_eq!(parse_loadavg("fast"), None);
        assert_eq!(parse_loadavg("-1 0 0"), None);
        assert_eq!(parse_loadavg(""), None);
    }
}
