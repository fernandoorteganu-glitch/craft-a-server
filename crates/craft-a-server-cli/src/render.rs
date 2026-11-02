//! Human-readable output. Pure functions: they return strings, never print.

use craft_a_server_core::ctl::{
    BackupInfo, DeviceInfo, DiagnoseInfo, EventInfo, ModChangeInfo, ModHitInfo, ModInstallPlanInfo,
    ModLineInfo, ModListInfo, ModRemovePlanInfo, ModUpdateInfo, ServerInfo,
};
use craft_a_server_core::tunnel::{TunnelInfo, TunnelState};

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

/// Pairing code as `XXXX-XXXX`, how long it lasts and where to enter it.
pub fn render_pairing(
    code: &str,
    expires_unix: i64,
    now_unix: i64,
    url: &str,
    alt_urls: &[String],
) -> String {
    let code = sanitize(code);
    let shown = match (code.get(..4), code.get(4..)) {
        (Some(a), Some(b)) if !b.is_empty() => format!("{a}-{b}"),
        _ => code,
    };
    let minutes = ((expires_unix - now_unix).max(0) + 59) / 60;
    let mut out = format!(
        "Pairing code: {shown}
Valid for {minutes} minute{} and works once.

         On your phone (same Wi-Fi), open:
  {}",
        if minutes == 1 { "" } else { "s" },
        sanitize(url)
    );
    for alt in alt_urls {
        out.push_str(&format!(
            "
  {}",
            sanitize(alt)
        ));
    }
    out
}

/// Online play: state, claim link, public address.
pub fn render_tunnel(info: &TunnelInfo) -> String {
    let mut out = format!("Online play: {}", if info.enabled { "on" } else { "off" });
    match &info.state {
        TunnelState::NotConfigured if info.enabled => {
            out.push_str(
                "
Preparing the playit agent (downloading and checking it)...",
            );
        }
        TunnelState::NotConfigured => {
            out.push_str(
                "
Not set up. Turn it on with: craft-a-server tunnel enable",
            );
        }
        TunnelState::Claiming { code, url } => {
            out.push_str(&format!(
                "
Link this machine to playit.gg: open
  {}
in a browser and accept (code {}).",
                sanitize(url),
                sanitize(code)
            ));
        }
        TunnelState::Claimed => {
            out.push_str(&format!(
                "
Linked to playit.gg, waiting for a Minecraft Java tunnel. In the playit                  dashboard create one that forwards to 127.0.0.1:{}.",
                info.local_port
            ));
            if !info.enabled {
                out.push_str(
                    "
The link is kept; turn it on again with: craft-a-server tunnel enable",
                );
            }
        }
        TunnelState::Running { address } => {
            out.push_str(&format!(
                "
Friends can join at: {}",
                sanitize(address)
            ));
        }
        TunnelState::Error { reason } => {
            out.push_str(&format!(
                "
Problem: {}",
                sanitize(reason)
            ));
            if info.enabled {
                out.push_str(
                    "
It is retried automatically.",
                );
            }
        }
    }
    out
}

/// The claim link alone (or why there is none).
pub fn render_claim_url(info: &TunnelInfo) -> String {
    match &info.state {
        TunnelState::Claiming { url, code } => format!(
            "Open this link in a browser to link this machine to playit.gg (code {}):
{}",
            sanitize(code),
            sanitize(url)
        ),
        _ if !info.enabled => {
            "Online play is off. Turn it on with: craft-a-server tunnel enable".to_owned()
        }
        TunnelState::NotConfigured => {
            "The link is not ready yet. Try again in a few seconds.".to_owned()
        }
        _ => "No link is needed right now. See: craft-a-server tunnel status".to_owned(),
    }
}

/// Paired devices as a table.
pub fn render_devices(list: &[DeviceInfo]) -> String {
    if list.is_empty() {
        return "No paired devices. Run `craft-a-server pair` to add one.".to_owned();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|d| {
            vec![
                sanitize(&d.id),
                sanitize(&d.device_name),
                format_unix(d.last_seen_unix),
                format_unix(d.expires_unix),
            ]
        })
        .collect();
    table(&["ID", "DEVICE", "LAST SEEN", "SIGNED IN UNTIL"], &rows)
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

pub fn render_mod_hits(hits: &[ModHitInfo]) -> String {
    if hits.is_empty() {
        return "No mods found for this server's Minecraft version.".to_owned();
    }
    let rows: Vec<Vec<String>> = hits
        .iter()
        .map(|h| {
            vec![
                sanitize(&h.project_id),
                sanitize(&h.title),
                format!("server {}", sanitize(&h.server_side)),
                h.downloads.to_string(),
                if h.installed { "installed" } else { "" }.to_owned(),
                sanitize(&h.description).chars().take(60).collect(),
            ]
        })
        .collect();
    table(
        &["ID", "NAME", "SIDE", "DOWNLOADS", "", "DESCRIPTION"],
        &rows,
    )
}

pub fn render_mod_list(list: &ModListInfo) -> String {
    let mut out = if list.mods.is_empty() {
        "No mods installed through craft-a-server.".to_owned()
    } else {
        let rows: Vec<Vec<String>> = list
            .mods
            .iter()
            .map(|m| {
                vec![
                    sanitize(&m.project_id),
                    sanitize(&m.title),
                    sanitize(&m.version_number),
                    if m.as_dependency { "dependency" } else { "" }.to_owned(),
                    sanitize(&m.status),
                    sanitize(&m.required_by.join(", ")),
                ]
            })
            .collect();
        table(
            &["ID", "NAME", "VERSION", "KIND", "STATUS", "NEEDED BY"],
            &rows,
        )
    };
    if !list.untracked.is_empty() {
        out.push_str("\nOther jars in the mods folder (not managed here):");
        for name in &list.untracked {
            out.push_str("\n  ");
            out.push_str(&sanitize(name));
        }
    }
    out
}

fn mod_line(l: &ModLineInfo) -> String {
    format!(
        "{} {} ({}, {})",
        sanitize(&l.title),
        sanitize(&l.version_number),
        sanitize(&l.version_type),
        format_size(l.size_bytes)
    )
}

pub fn render_mod_plan(p: &ModInstallPlanInfo) -> String {
    let mut out = format!(
        "{} {}\n  {}",
        if p.replaces.is_some() {
            "Update"
        } else {
            "Install"
        },
        mod_line(&p.target),
        match &p.replaces {
            Some(old) => format!("replaces {}", sanitize(old)),
            None => format!("for Minecraft {}", sanitize(&p.mc_version)),
        }
    );
    if !p.dependencies.is_empty() {
        out.push_str("\nRequired mods that will be installed too:");
        for d in &p.dependencies {
            out.push_str("\n  ");
            out.push_str(&mod_line(d));
        }
    }
    for (label, list) in [
        ("Already installed", &p.already_installed),
        ("Optional, not installed", &p.optional),
        ("CANNOT INSTALL", &p.conflicts),
    ] {
        if !list.is_empty() {
            let joined: Vec<String> = list.iter().map(|s| sanitize(s)).collect();
            out.push_str(&format!("\n{label}: {}", joined.join("; ")));
        }
    }
    if p.server_running {
        out.push_str(
            "\nThe server is running: it will be stopped, backed up, restarted with the change, \
             and rolled back if it does not come up.",
        );
    }
    out
}

pub fn render_mod_remove_plan(p: &ModRemovePlanInfo) -> String {
    let mut out = format!("Remove {}", sanitize(&p.title));
    if !p.dependents.is_empty() {
        out.push_str(&format!(
            "\nWARNING: needed by {}",
            sanitize(&p.dependents.join(", "))
        ));
    }
    if !p.orphans.is_empty() {
        out.push_str(&format!(
            "\nNo longer needed afterwards: {}",
            sanitize(&p.orphans.join(", "))
        ));
    }
    out
}

pub fn render_mod_updates(list: &[ModUpdateInfo]) -> String {
    if list.is_empty() {
        return "All mods are up to date.".to_owned();
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|u| {
            vec![
                sanitize(&u.project_id),
                sanitize(&u.title),
                sanitize(&u.installed_version),
                sanitize(&u.latest_version),
            ]
        })
        .collect();
    format!(
        "{}\nApply one with: craft-a-server mods install <server> <ID>",
        table(&["ID", "NAME", "INSTALLED", "LATEST"], &rows)
    )
}

pub fn render_mod_change(c: &ModChangeInfo) -> String {
    let mut parts = Vec::new();
    for i in &c.installed {
        parts.push(format!("Installed {}", mod_line(i)));
    }
    for r in &c.removed {
        parts.push(format!("Removed {}", sanitize(r)));
    }
    parts.push(
        if c.verified {
            "The server restarted and answered, so the change works."
        } else {
            "The server is stopped, so the change was not tried out. It takes effect at the next start."
        }
        .to_owned(),
    );
    if let Some(b) = &c.backup_id {
        parts.push(format!("Backup taken first: {}", sanitize(b)));
    }
    parts.join("\n")
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
    use craft_a_server_core::ctl::InstalledModInfo;

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
    fn pairing_is_shown_with_a_dash_and_minutes() {
        let text = render_pairing(
            "ABCD2345",
            1_000 + 600,
            1_000,
            "http://h/pair",
            &["http://1.2.3.4/pair".into()],
        );
        assert!(text.contains("ABCD-2345"), "{text}");
        assert!(text.contains("10 minutes"), "{text}");
        assert!(text.contains("http://h/pair") && text.contains("http://1.2.3.4/pair"));
        assert!(render_pairing("AB", 0, 100, "u", &[]).contains("0 minutes"));
    }

    #[test]
    fn devices_table() {
        assert!(render_devices(&[]).contains("No paired devices"));
        let d = DeviceInfo {
            id: "d1".into(),
            device_name: "Pixel[31m".into(),
            created_unix: 0,
            last_seen_unix: 0,
            expires_unix: 0,
        };
        let text = render_devices(&[d]);
        assert!(text.contains("d1") && text.contains("Pixel?[31m"), "{text}");
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

    fn line(title: &str) -> ModLineInfo {
        ModLineInfo {
            project_id: "AAAAAAAA".into(),
            title: title.into(),
            version_number: "1.0.0".into(),
            version_type: "release".into(),
            size_bytes: 2048,
            file_name: "a.jar".into(),
            as_dependency: false,
        }
    }

    #[test]
    fn mod_search_results_strip_terminal_escapes() {
        assert!(render_mod_hits(&[]).contains("No mods found"));
        let h = ModHitInfo {
            project_id: "AAAAAAAA".into(),
            slug: "a".into(),
            title: "Evil\u{1b}[31m".into(),
            description: "d".into(),
            author: "me".into(),
            downloads: 42,
            server_side: "optional".into(),
            installed: true,
        };
        let out = render_mod_hits(&[h]);
        assert!(
            out.contains("Evil?[31m") && out.contains("installed"),
            "{out}"
        );
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn install_plan_lists_dependencies_conflicts_and_the_restart() {
        let plan = ModInstallPlanInfo {
            server_id: "s1".into(),
            mc_version: "26.3".into(),
            server_running: true,
            target: line("Alpha"),
            dependencies: vec![line("Beta Lib")],
            already_installed: vec!["Core".into()],
            optional: vec!["Extra".into()],
            conflicts: vec!["Alpha cannot run together with Foe".into()],
            replaces: None,
            unresolved: 0,
        };
        let out = render_mod_plan(&plan);
        for needle in [
            "Install Alpha 1.0.0",
            "Beta Lib",
            "Already installed: Core",
            "Optional, not installed: Extra",
            "CANNOT INSTALL",
            "stopped, backed up, restarted",
        ] {
            assert!(out.contains(needle), "{needle}: {out}");
        }
        let mut update = plan;
        update.replaces = Some("Alpha 0.9".into());
        update.server_running = false;
        let out = render_mod_plan(&update);
        assert!(
            out.starts_with("Update") && out.contains("replaces Alpha 0.9"),
            "{out}"
        );
        assert!(!out.contains("restarted"));
    }

    #[test]
    fn removal_warns_about_dependents() {
        let plan = ModRemovePlanInfo {
            server_id: "s1".into(),
            project_id: "BBBBBBBB".into(),
            title: "Beta Lib".into(),
            server_running: false,
            dependents: vec!["Alpha".into()],
            orphans: vec!["Tiny".into()],
        };
        let out = render_mod_remove_plan(&plan);
        assert!(
            out.contains("WARNING: needed by Alpha") && out.contains("Tiny"),
            "{out}"
        );
    }

    #[test]
    fn change_results_say_whether_the_server_was_checked() {
        let mut c = ModChangeInfo {
            installed: vec![line("Alpha")],
            removed: vec!["Old".into()],
            verified: true,
            backup_id: Some("b1".into()),
        };
        let out = render_mod_change(&c);
        assert!(
            out.contains("Installed Alpha") && out.contains("Removed Old"),
            "{out}"
        );
        assert!(out.contains("answered"));
        c.verified = false;
        assert!(render_mod_change(&c).contains("not tried out"));
    }

    #[test]
    fn installed_list_and_updates() {
        assert!(
            render_mod_list(&ModListInfo {
                mods: vec![],
                untracked: vec![]
            })
            .contains("No mods")
        );
        let list = ModListInfo {
            mods: vec![InstalledModInfo {
                project_id: "AAAAAAAA".into(),
                title: "Alpha".into(),
                version_number: "1".into(),
                file_name: "a.jar".into(),
                size_bytes: 1,
                as_dependency: true,
                installed_unix: 0,
                status: "missing".into(),
                required_by: vec!["Beta".into()],
            }],
            untracked: vec!["mine.jar".into()],
        };
        let out = render_mod_list(&list);
        assert!(
            out.contains("dependency") && out.contains("missing") && out.contains("mine.jar"),
            "{out}"
        );
        assert!(render_mod_updates(&[]).contains("up to date"));
        let u = ModUpdateInfo {
            project_id: "AAAAAAAA".into(),
            title: "Alpha".into(),
            installed_version: "1".into(),
            latest_version: "2".into(),
        };
        assert!(render_mod_updates(&[u]).contains("craft-a-server mods install"));
    }

    fn tunnel(enabled: bool, state: TunnelState) -> TunnelInfo {
        TunnelInfo {
            enabled,
            state,
            consent_unix: None,
            updated_unix: 0,
            agent_version: "1.2.0".into(),
            local_port: 25565,
        }
    }

    #[test]
    fn tunnel_output_covers_every_state_and_hides_control_characters() {
        let off = render_tunnel(&tunnel(false, TunnelState::NotConfigured));
        assert!(
            off.contains("off") && off.contains("tunnel enable"),
            "{off}"
        );
        let prep = render_tunnel(&tunnel(true, TunnelState::NotConfigured));
        assert!(prep.contains("Preparing"), "{prep}");
        let claiming = tunnel(
            true,
            TunnelState::Claiming {
                code: "abc123".into(),
                url: "https://playit.gg/claim/abc123".into(),
            },
        );
        let out = render_tunnel(&claiming);
        assert!(out.contains("https://playit.gg/claim/abc123"), "{out}");
        assert!(render_claim_url(&claiming).contains("https://playit.gg/claim/abc123"));
        let claimed = render_tunnel(&tunnel(true, TunnelState::Claimed));
        assert!(claimed.contains("127.0.0.1:25565"), "{claimed}");
        let running = tunnel(
            true,
            TunnelState::Running {
                address: "mine.craft.playit.gg".into(),
            },
        );
        assert!(render_tunnel(&running).contains("mine.craft.playit.gg"));
        assert!(render_claim_url(&running).contains("No link is needed"));
        assert!(render_claim_url(&tunnel(false, TunnelState::NotConfigured)).contains("off"));
        let err = render_tunnel(&tunnel(
            true,
            TunnelState::Error {
                reason: "bad \u{1b}[31mthing".into(),
            },
        ));
        assert!(err.contains("Problem") && !err.contains('\u{1b}'), "{err}");
    }
}
