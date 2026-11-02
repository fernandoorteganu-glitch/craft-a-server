//! The Performance screen: template bodies and the pure code that turns a
//! [`Resources`] snapshot into the text shown.

use askama::Template;

use super::activity::Banner;
use super::strings as s;
use super::views::format_bytes;
use crate::lifecycle::Resources;
use crate::settings::{VIEW_DISTANCE_DEFAULT, VIEW_DISTANCE_MAX, VIEW_DISTANCE_MIN};

/// Warn when less than this share of the machine's memory is left.
const LOW_MEMORY_PERCENT: u64 = 10;
/// Warn when less than this share of the disk is free, or under 2 GiB.
const LOW_DISK_PERCENT: u64 = 10;
const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Template, Debug)]
#[template(path = "performance.html")]
pub struct PerformanceBody<'a> {
    pub usage: &'a str,
    pub distance: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GameView {
    pub memory_text: String,
    pub memory_limit_text: Option<String>,
    pub memory_percent: Option<u64>,
    pub heap_text: Option<String>,
    pub cpu_text: Option<String>,
    pub pids_text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct HostView {
    pub memory_text: String,
    pub memory_percent: u64,
    pub load_text: String,
    pub disk_text: Option<String>,
    pub warnings: Vec<&'static str>,
}

/// The polled "right now" card.
#[derive(Template, Debug)]
#[template(path = "performance_usage.html")]
pub struct UsageBody {
    pub has_server: bool,
    pub running: bool,
    pub game: Option<GameView>,
    pub host: Option<HostView>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Choice {
    pub value: u8,
    pub selected: bool,
    /// The game's own default is marked as such.
    pub is_default: bool,
}

/// The view-distance form.
#[derive(Template, Debug)]
#[template(path = "performance_distance.html")]
pub struct DistanceBody<'a> {
    pub has_server: bool,
    pub choices: Vec<Choice>,
    pub csrf_token: &'a str,
    pub banner: Banner,
}

impl DistanceBody<'_> {
    pub fn new(current: Option<u8>, csrf_token: &str, banner: Banner) -> DistanceBody<'_> {
        // Nothing chosen yet means the game is using its own default.
        let shown = current.unwrap_or(VIEW_DISTANCE_DEFAULT);
        DistanceBody {
            has_server: true,
            choices: (VIEW_DISTANCE_MIN..=VIEW_DISTANCE_MAX)
                .map(|value| Choice {
                    value,
                    selected: value == shown,
                    is_default: value == VIEW_DISTANCE_DEFAULT,
                })
                .collect(),
            csrf_token,
            banner,
        }
    }

    pub fn without_server(csrf_token: &str, banner: Banner) -> DistanceBody<'_> {
        DistanceBody {
            has_server: false,
            ..DistanceBody::new(None, csrf_token, banner)
        }
    }
}

impl UsageBody {
    pub fn without_server() -> UsageBody {
        UsageBody {
            has_server: false,
            running: false,
            game: None,
            host: None,
        }
    }

    pub fn from_resources(r: &Resources) -> UsageBody {
        UsageBody {
            has_server: true,
            running: r.running,
            game: if r.running { game_view(r) } else { None },
            host: host_view(r),
        }
    }
}

fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        0
    } else {
        u64::try_from((u128::from(part) * 100 / u128::from(whole)).min(100)).unwrap_or(100)
    }
}

fn game_view(r: &Resources) -> Option<GameView> {
    let g = r.game?;
    let cpu_text = g.cpu_cores.map(|cores| {
        let cpus = r.cpus.max(1);
        let share = (cores / f64::from(cpus) * 100.0).clamp(0.0, 100.0);
        let unit = if cpus == 1 { "core" } else { "cores" };
        format!("{share:.0}% ({cores:.1} of {cpus} {unit})")
    });
    Some(GameView {
        memory_text: format_bytes(g.mem_used_bytes),
        memory_limit_text: r.memory_limit_bytes.map(format_bytes),
        memory_percent: r
            .memory_limit_bytes
            .map(|limit| percent(g.mem_used_bytes, limit)),
        heap_text: r.heap_bytes.map(format_bytes),
        cpu_text,
        pids_text: format!("{} (limit {})", g.pids, r.pids_limit),
    })
}

fn host_view(r: &Resources) -> Option<HostView> {
    let h = r.host?;
    let used = h.mem_total_bytes.saturating_sub(h.mem_available_bytes);
    let mut warnings = Vec::new();
    if percent(h.mem_available_bytes, h.mem_total_bytes) < LOW_MEMORY_PERCENT {
        warnings.push(s::PERF_WARN_MEMORY);
    }
    let disk_text = r.disk.map(|d| {
        format!(
            "{} of {}",
            format_bytes(d.free_bytes),
            format_bytes(d.total_bytes)
        )
    });
    if r.disk.is_some_and(|d| {
        d.free_bytes < LOW_DISK_BYTES || percent(d.free_bytes, d.total_bytes) < LOW_DISK_PERCENT
    }) {
        warnings.push(s::PERF_WARN_DISK);
    }
    Some(HostView {
        memory_text: format!(
            "{} of {}",
            format_bytes(used),
            format_bytes(h.mem_total_bytes)
        ),
        memory_percent: percent(used, h.mem_total_bytes),
        load_text: format!("{:.2}", h.load1),
        disk_text,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{DiskUsage, GameUse, HostUse};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn base() -> Resources {
        Resources {
            running: true,
            game: Some(GameUse {
                mem_used_bytes: GIB,
                cpu_cores: Some(0.5),
                pids: 40,
            }),
            memory_limit_bytes: Some(4 * GIB),
            heap_bytes: Some(3 * GIB),
            cpus: 2,
            pids_limit: 512,
            host: Some(HostUse {
                mem_total_bytes: 8 * GIB,
                mem_available_bytes: 4 * GIB,
                load1: 0.5,
            }),
            disk: Some(DiskUsage {
                free_bytes: 50 * GIB,
                total_bytes: 100 * GIB,
            }),
        }
    }

    #[test]
    fn a_running_server_shows_game_and_machine() -> Result<(), &'static str> {
        let v = UsageBody::from_resources(&base());
        let g = v.game.ok_or("no game view")?;
        assert_eq!(g.memory_text, "1.0 GiB");
        assert_eq!(g.memory_limit_text.as_deref(), Some("4.0 GiB"));
        assert_eq!(g.memory_percent, Some(25));
        assert_eq!(g.heap_text.as_deref(), Some("3.0 GiB"));
        assert_eq!(g.cpu_text.as_deref(), Some("25% (0.5 of 2 cores)"));
        assert_eq!(g.pids_text, "40 (limit 512)");
        let h = v.host.ok_or("no host view")?;
        assert_eq!(h.memory_text, "4.0 GiB of 8.0 GiB");
        assert_eq!(h.memory_percent, 50);
        assert_eq!(h.load_text, "0.50");
        assert_eq!(h.disk_text.as_deref(), Some("50.0 GiB of 100.0 GiB"));
        assert!(h.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn the_first_cpu_reading_has_no_text() -> Result<(), &'static str> {
        let mut r = base();
        r.game = r.game.map(|g| GameUse {
            cpu_cores: None,
            ..g
        });
        let g = UsageBody::from_resources(&r).game.ok_or("no game view")?;
        assert_eq!(g.cpu_text, None);
        Ok(())
    }

    #[test]
    fn a_stopped_server_shows_no_game_numbers_but_still_the_machine() {
        let mut r = base();
        r.running = false;
        r.game = None;
        let v = UsageBody::from_resources(&r);
        assert!(v.game.is_none());
        assert!(v.host.is_some());
        assert!(!v.running);
    }

    #[test]
    fn low_memory_and_disk_raise_warnings() -> Result<(), &'static str> {
        let mut r = base();
        r.host = Some(HostUse {
            mem_total_bytes: 8 * GIB,
            mem_available_bytes: GIB / 2,
            load1: 3.0,
        });
        r.disk = Some(DiskUsage {
            free_bytes: GIB,
            total_bytes: 100 * GIB,
        });
        let h = host_view(&r).ok_or("no host view")?;
        assert_eq!(h.warnings, vec![s::PERF_WARN_MEMORY, s::PERF_WARN_DISK]);
        Ok(())
    }

    #[test]
    fn percentages_never_exceed_100_or_divide_by_zero() {
        assert_eq!(percent(5, 0), 0);
        assert_eq!(percent(200, 100), 100);
        assert_eq!(percent(u64::MAX, u64::MAX), 100);
    }

    #[test]
    fn the_form_offers_the_allowed_range_and_marks_the_current_value() {
        let b = DistanceBody::new(Some(8), "t", Banner::default());
        assert_eq!(b.choices.first().map(|c| c.value), Some(VIEW_DISTANCE_MIN));
        assert_eq!(b.choices.last().map(|c| c.value), Some(VIEW_DISTANCE_MAX));
        assert_eq!(b.choices.iter().filter(|c| c.selected).count(), 1);
        assert!(b.choices.iter().any(|c| c.value == 8 && c.selected));
        assert_eq!(b.choices.iter().filter(|c| c.is_default).count(), 1);
        // Never chosen: the game's default is the one shown as current.
        let unset = DistanceBody::new(None, "t", Banner::default());
        assert!(
            unset
                .choices
                .iter()
                .any(|c| c.value == VIEW_DISTANCE_DEFAULT && c.selected)
        );
    }
}
