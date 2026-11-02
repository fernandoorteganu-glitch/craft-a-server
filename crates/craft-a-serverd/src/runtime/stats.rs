//! Parsing of `podman stats --no-stream --format json` output.
//!
//! Podman prints human-readable strings (`"30.83MB / 16.18GB"`, `"31m44.6s"`,
//! `"26"`), so they are parsed here, strictly: anything unexpected is a parse
//! error, never a made-up number. The CPU percentage podman prints is not used
//! (it is meaningless for a single sample); the cumulative CPU time is, and the
//! caller turns two readings into a rate.

use std::time::Duration;

use serde::Deserialize;

use super::{ContainerStats, RuntimeError};

#[derive(Deserialize)]
struct RawStats {
    #[serde(default)]
    mem_usage: String,
    #[serde(default)]
    cpu_time: String,
    #[serde(default)]
    pids: String,
}

/// Parses the JSON array `podman stats` prints for one container.
pub(crate) fn parse_stats(json: &str) -> Result<ContainerStats, RuntimeError> {
    let bad = |what: &str| RuntimeError::Parse(format!("podman stats: {what}"));
    let raws: Vec<RawStats> = serde_json::from_str(json).map_err(|e| bad(&e.to_string()))?;
    let raw = raws
        .first()
        .ok_or_else(|| bad("no container in the output"))?;
    let used = raw
        .mem_usage
        .split('/')
        .next()
        .ok_or_else(|| bad("no memory usage"))?;
    Ok(ContainerStats {
        mem_used_bytes: parse_size(used).ok_or_else(|| bad("unreadable memory usage"))?,
        cpu_time: parse_go_duration(&raw.cpu_time).ok_or_else(|| bad("unreadable CPU time"))?,
        pids: raw
            .pids
            .trim()
            .parse()
            .map_err(|_| bad("unreadable process count"))?,
    })
}

/// `"30.83MB"`, `"512kB"`, `"1.5GiB"`, `"0B"` to bytes. Decimal units for
/// `kB`/`MB`/`GB`, binary for `KiB`/`MiB`/`GiB`.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let factor: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = value * factor;
    (bytes.is_finite() && (0.0..1e18).contains(&bytes)).then_some(bytes as u64)
}

/// Go's `time.Duration` text: `"31m44.607025s"`, `"1h2m3s"`, `"250ms"`, `"0s"`.
fn parse_go_duration(text: &str) -> Option<Duration> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0_f64;
    while !rest.is_empty() {
        let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (number, tail) = rest.split_at(end);
        let value: f64 = number.parse().ok()?;
        // Longest unit first so "ms" is not read as "m" followed by "s".
        let (seconds, len) = [
            ("ns", 1e-9),
            ("us", 1e-6),
            ("µs", 1e-6),
            ("ms", 1e-3),
            ("h", 3600.0),
            ("m", 60.0),
            ("s", 1.0),
        ]
        .iter()
        .find(|(unit, _)| tail.starts_with(unit))
        .map(|(unit, factor)| (*factor, unit.len()))?;
        total += value * seconds;
        rest = tail.get(len..)?;
    }
    (total.is_finite() && (0.0..1e12).contains(&total)).then(|| Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_podman_output() -> Result<(), RuntimeError> {
        let json = r#"[
 {
  "id": "eb15a7228ab4",
  "name": "statstest",
  "cpu_time": "31m44.607025s",
  "cpu_percent": "54823.02%",
  "avg_cpu": "54823.02%",
  "mem_usage": "30.83MB / 16.18GB",
  "mem_percent": "0.19%",
  "net_io": "0B / 516B",
  "block_io": "0B / 0B",
  "pids": "26"
 }
]"#;
        let s = parse_stats(json)?;
        assert_eq!(s.mem_used_bytes, 30_830_000);
        assert_eq!(s.pids, 26);
        assert_eq!(
            s.cpu_time,
            Duration::from_secs_f64(31.0 * 60.0 + 44.607_025)
        );
        Ok(())
    }

    #[test]
    fn rejects_garbage_instead_of_inventing_numbers() {
        for json in [
            "",
            "[]",
            "{}",
            r#"[{"mem_usage":"lots","cpu_time":"1s","pids":"1"}]"#,
            r#"[{"mem_usage":"1MB / 2MB","cpu_time":"soon","pids":"1"}]"#,
            r#"[{"mem_usage":"1MB / 2MB","cpu_time":"1s","pids":"many"}]"#,
            r#"[{"cpu_time":"1s","pids":"1"}]"#,
        ] {
            assert!(parse_stats(json).is_err(), "accepted {json:?}");
        }
    }

    #[test]
    fn sizes_use_the_right_units() {
        assert_eq!(parse_size("0B"), Some(0));
        assert_eq!(parse_size("512kB"), Some(512_000));
        assert_eq!(parse_size("1.5GiB"), Some(1_610_612_736));
        assert_eq!(parse_size("2MiB"), Some(2_097_152));
        assert_eq!(parse_size("12"), None);
        assert_eq!(parse_size("MB"), None);
        assert_eq!(parse_size("5 parsecs"), None);
    }

    #[test]
    fn durations_follow_go_syntax() {
        assert_eq!(parse_go_duration("0s"), Some(Duration::ZERO));
        assert_eq!(parse_go_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_go_duration("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_go_duration("12µs"), Some(Duration::from_micros(12)));
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("3"), None);
        assert_eq!(parse_go_duration("3 s"), None);
    }
}
