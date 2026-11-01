//! Human-readable output. Pure functions: they return strings, never print.

use craft_a_server_core::ctl::{BackupInfo, DiagnoseInfo, EventInfo, ServerInfo};

/// Replaces control characters (terminal escape sequences, bells...) so text
/// that came from a container log or a player name cannot drive the terminal.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() && c != '\t' { '?' } else { c })
        .collect()
}

/// Like [`sanitize`] but keeps line breaks, for multi-line text such as logs.
pub fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\t' && c != '\n' {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Left-aligned table with a header row.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut out = String::new();
        for (i, (cell, w)) in cells.iter().zip(&widths).enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(cell);
            if i + 1 < widths.len() {
                for _ in cell.chars().count()..*w {
                    out.push(' ');
                }
            }
        }
        out.trim_end().to_owned()
    };
    let mut out = line(headers.to_vec());
    for row in rows {
        out.push('\n');
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

/// `2026-11-01 12:00:00 UTC`.
pub fn format_unix(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `1.5 MiB`, `312 B`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS.get(unit).copied().unwrap_or("TiB"))
}

pub fn render_servers(servers: &[ServerInfo]) -> String {
    if servers.is_empty() {
        return "No servers yet. Create one with: craft-a-server create <name> --version <mc> --accept-eula".to_owned();
    }
    let rows: Vec<Vec<String>> = servers
        .iter()
        .map(|s| {
            vec![
                sanitize(&s.id),
                sanitize(&s.name),
                sanitize(&s.mc_version),
                sanitize(&s.state),
                s.port.to_string(),
                format_unix(s.created_unix),
            ]
        })
        .collect();
    table(
        &["ID", "NAME", "VERSION", "STATE", "PORT", "CREATED"],
        &rows,
    )
}

pub fn render_server(s: &ServerInfo) -> String {
    let mut out = format!(
        "ID:       {}\nName:     {}\nVersion:  {}\nState:    {}\nPort:     {}\nCreated:  {}",
        sanitize(&s.id),
        sanitize(&s.name),
        sanitize(&s.mc_version),
        sanitize(&s.state),
        s.port,
        format_unix(s.created_unix)
    );
    if let Some(reason) = &s.reason {
        out.push_str(&format!(
            "\nReason:   {}\nHint:     fix the cause, then run: craft-a-server reset {}",
            sanitize(reason),
            sanitize(&s.id)
        ));
    }
    out
}

pub fn render_backups(backups: &[BackupInfo]) -> String {
    if backups.is_empty() {
        return "No backups yet.".to_owned();
    }
    let rows: Vec<Vec<String>> = backups
        .iter()
        .map(|b| {
            vec![
                sanitize(&b.id),
                sanitize(&b.kind),
                format_size(b.size_bytes),
                format_unix(b.created_unix),
                if b.verified { "yes" } else { "NO" }.to_owned(),
            ]
        })
        .collect();
    table(&["BACKUP", "KIND", "SIZE", "CREATED", "VERIFIED"], &rows)
}

fn render_event(e: &EventInfo) -> String {
    let server = e.server_id.as_deref().unwrap_or("-");
    format!(
        "{}  {}  {}  {}",
        format_unix(e.ts_unix),
        sanitize(server),
        sanitize(&e.kind),
        sanitize(&e.detail)
    )
}

/// The diagnose report. `cli_version`/`cli_protocol` describe this binary.
pub fn render_diagnose(d: &DiagnoseInfo, cli_version: &str, cli_protocol: u32) -> String {
    let mut out = format!(
        "craft-a-server CLI:  {cli_version} (protocol {cli_protocol})\n\
         craft-a-serverd:     {} (protocol {})\n\
         Podman:              {}\n\
         Database schema:     {}\n",
        sanitize(&d.daemon_version),
        d.protocol,
        d.podman_version
            .as_deref()
            .map_or_else(|| "not available".to_owned(), sanitize),
        d.schema_version
    );
    if d.protocol != cli_protocol {
        out.push_str("WARNING: the CLI and the daemon speak different protocol versions.\n");
    }
    out.push_str("\nServers:\n");
    out.push_str(&render_servers(&d.servers));
    out.push_str("\n\nRecent events (newest first):\n");
    if d.events.is_empty() {
        out.push_str("  none\n");
    }
    for e in &d.events {
        out.push_str("  ");
        out.push_str(&render_event(e));
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// The report when the daemon cannot be reached.
pub fn render_unreachable(
    socket: &str,
    error: &str,
    cli_version: &str,
    cli_protocol: u32,
) -> String {
    format!(
        "craft-a-server CLI:  {cli_version} (protocol {cli_protocol})\n\
         daemon not reachable at {socket}: {error}\n\
         Is craft-a-serverd running? Try: systemctl status craft-a-serverd"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(id: &str, state: &str, reason: Option<&str>) -> ServerInfo {
        ServerInfo {
            id: id.into(),
            name: "Survival".into(),
            mc_version: "26.3".into(),
            state: state.into(),
            reason: reason.map(str::to_owned),
            port: 25565,
            created_unix: 1_700_000_000,
        }
    }

    #[test]
    fn unix_times() {
        assert_eq!(format_unix(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_unix(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert_eq!(format_unix(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_unix(-1), "1969-12-31 23:59:59 UTC");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536 * 1024), "1.5 MiB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn sanitize_strips_escapes() {
        assert_eq!(sanitize("a\x1b[31mred\x07"), "a?[31mred?");
        assert_eq!(sanitize("tab\there"), "tab\there");
        assert_eq!(sanitize("a\nb"), "a?b");
        assert_eq!(sanitize_text("a\nb\x1b"), "a\nb?");
    }

    #[test]
    fn table_aligns_columns() {
        let t = table(
            &["ID", "NAME"],
            &[
                vec!["s1".into(), "A".into()],
                vec!["s2222".into(), "Bee".into()],
            ],
        );
        assert_eq!(t, "ID     NAME\ns1     A\ns2222  Bee");
    }

    #[test]
    fn server_list() {
        let out = render_servers(&[server("s1", "running", None), server("s2", "stopped", None)]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines.first().is_some_and(|l| l.starts_with("ID")));
        assert!(
            lines
                .get(1)
                .is_some_and(|l| l.contains("running") && l.contains("25565"))
        );
        assert!(render_servers(&[]).contains("No servers yet"));
    }

    #[test]
    fn failed_server_shows_reason_and_hint() {
        let out = render_server(&server(
            "s1",
            "failed",
            Some("server crashed (exit code 1)"),
        ));
        assert!(out.contains("Reason:   server crashed (exit code 1)"));
        assert!(out.contains("craft-a-server reset s1"));
        assert!(!render_server(&server("s1", "running", None)).contains("Reason"));
    }

    #[test]
    fn backups_table() {
        let out = render_backups(&[BackupInfo {
            id: "b1".into(),
            server_id: "s1".into(),
            kind: "manual".into(),
            size_bytes: 2048,
            created_unix: 0,
            verified: false,
        }]);
        assert!(out.contains("2.0 KiB"));
        assert!(out.contains("NO"));
        assert_eq!(render_backups(&[]), "No backups yet.");
    }

    #[test]
    fn diagnose_report() {
        let d = DiagnoseInfo {
            daemon_version: "0.0.1".into(),
            protocol: 1,
            podman_version: Some("5.4.2".into()),
            schema_version: 1,
            servers: vec![server("s1", "running", None)],
            events: vec![EventInfo {
                ts_unix: 0,
                server_id: Some("s1".into()),
                kind: "state".into(),
                detail: "starting -> running".into(),
            }],
        };
        let out = render_diagnose(&d, "0.0.1", 1);
        assert!(out.contains("Podman:              5.4.2"));
        assert!(out.contains("starting -> running"));
        assert!(!out.contains("WARNING"));
        assert!(render_diagnose(&d, "0.0.1", 2).contains("WARNING"));
    }

    #[test]
    fn unreachable_report_names_socket_and_error() {
        let out = render_unreachable("/run/x.sock", "No such file", "0.0.1", 1);
        assert!(out.contains("daemon not reachable at /run/x.sock: No such file"));
    }
}
