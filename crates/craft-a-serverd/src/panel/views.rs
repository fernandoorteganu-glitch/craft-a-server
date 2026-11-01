//! Template bodies of the screens and the pure helpers that prepare data for
//! them (formatting, log cleaning, whitelist parsing, the QR code).
//!
//! Everything shown is escaped by the templates; the only `|safe` values are
//! HTML this module or a template produced itself (a rendered partial, the QR
//! SVG built from booleans).

use std::fmt::Write as _;

use askama::Template;
use craft_a_server_core::config::DEFAULT_MC_PORT;
use craft_a_server_core::mc_protocol::ServerStatus;
use craft_a_server_core::model::ServerState;

use super::activity::Banner;
use super::strings as s;
use crate::db::{BackupRow, ServerRow};
use crate::lifecycle::player_name;

/// Longest log line shown, in characters.
pub const MAX_LOG_LINE_CHARS: usize = 400;
/// Log lines per refresh.
pub const LOG_LINES: usize = 200;
/// Most whitelist entries listed.
pub const MAX_WHITELIST_SHOWN: usize = 200;
/// Longest server reply echoed to the person, in characters.
pub const MAX_REPLY_CHARS: usize = 160;

const MIB: u64 = 1024 * 1024;

// ------------------------------------------------------------------ bodies

/// The server card (also the htmx poll target).
#[derive(Template, Debug)]
#[template(path = "card.html")]
pub struct CardBody<'a> {
    pub server: Option<ServerView>,
    pub banner: Banner,
    pub csrf_token: &'a str,
}

/// Home: the card plus who is signed in.
#[derive(Template, Debug)]
#[template(path = "home.html")]
pub struct HomeBody<'a> {
    pub card: &'a str,
    pub device_name: &'a str,
    pub csrf_token: &'a str,
}

#[derive(Debug)]
pub struct ServerView {
    pub name: String,
    pub mc_version: String,
    pub state_key: &'static str,
    pub state_label: &'static str,
    pub failure: Option<String>,
    pub players: Option<String>,
    pub address_local: String,
    pub address_ip: Option<String>,
    pub can_start: bool,
    pub can_stop: bool,
    pub can_restart: bool,
    pub can_reset: bool,
    pub can_delete: bool,
}

impl ServerView {
    pub fn new(
        row: &ServerRow,
        status: Option<&ServerStatus>,
        panel_hostname: &str,
        lan_ip: Option<std::net::Ipv4Addr>,
        busy: bool,
    ) -> ServerView {
        let (state_key, state_label, failure) = match &row.state {
            ServerState::Creating => ("creating", s::STATE_CREATING, None),
            ServerState::Stopped => ("stopped", s::STATE_STOPPED, None),
            ServerState::Starting => ("starting", s::STATE_STARTING, None),
            ServerState::Running => ("running", s::STATE_RUNNING, None),
            ServerState::Stopping => ("stopping", s::STATE_STOPPING, None),
            ServerState::Failed(reason) => ("failed", s::STATE_FAILED, Some(clean_text(reason))),
        };
        let stopped = row.state == ServerState::Stopped;
        let failed = matches!(row.state, ServerState::Failed(_));
        let running = row.state == ServerState::Running;
        ServerView {
            name: row.name.clone(),
            mc_version: row.mc_version.clone(),
            state_key,
            state_label,
            failure,
            players: status.map(|st| format!("{} / {}", st.players_online, st.players_max)),
            address_local: mc_address(panel_hostname, row.host_port, DEFAULT_MC_PORT),
            address_ip: lan_ip
                .map(|ip| mc_address(&ip.to_string(), row.host_port, DEFAULT_MC_PORT)),
            can_start: stopped && !busy,
            can_stop: running && !busy,
            can_restart: running && !busy,
            can_reset: failed && !busy,
            can_delete: (stopped || failed) && !busy,
        }
    }
}

#[derive(Template, Debug)]
#[template(path = "create.html")]
pub struct CreateBody<'a> {
    pub csrf_token: &'a str,
    pub error: Option<&'a str>,
    pub name: &'a str,
    pub mc_version: &'a str,
    pub memory: Option<MemoryView>,
}

#[derive(Debug)]
pub struct MemoryView {
    pub text: String,
    pub low: bool,
}

#[derive(Debug)]
pub struct BackupView {
    pub id: String,
    pub created: String,
    pub size: String,
    pub kind: &'static str,
}

impl BackupView {
    pub fn new(row: &BackupRow) -> BackupView {
        BackupView {
            id: row.id.clone(),
            created: super::pages::format_utc(row.created_unix),
            size: format_bytes(row.size_bytes),
            kind: backup_kind_label(&row.kind),
        }
    }
}

/// The backup list (also the htmx poll target).
#[derive(Template, Debug)]
#[template(path = "backups_list.html")]
pub struct BackupsList {
    pub backups: Vec<BackupView>,
    pub banner: Banner,
}

#[derive(Template, Debug)]
#[template(path = "backups.html")]
pub struct BackupsBody<'a> {
    pub list: &'a str,
    pub has_server: bool,
    pub busy: bool,
    pub csrf_token: &'a str,
}

/// A yes/no page for an action that cannot be undone.
#[derive(Template, Debug)]
#[template(path = "confirm.html")]
pub struct ConfirmBody<'a> {
    pub title: &'a str,
    pub warning: &'a str,
    pub detail: String,
    pub action: String,
    pub confirm_label: &'a str,
    pub cancel_href: &'a str,
    pub csrf_token: &'a str,
}

#[derive(Template, Debug)]
#[template(path = "players.html")]
pub struct PlayersBody<'a> {
    pub banner: Banner,
    pub csrf_token: &'a str,
    pub running: bool,
    pub players: Vec<String>,
}

#[derive(Template, Debug)]
#[template(path = "logs.html")]
pub struct LogsBody;

#[derive(Template, Debug)]
#[template(path = "logs_tail.html")]
pub struct LogsTail {
    pub lines: Vec<String>,
}

#[derive(Template, Debug)]
#[template(path = "mods.html")]
pub struct ModsBody;

/// One row of the devices list.
#[derive(Debug)]
pub struct DeviceRow {
    pub id: String,
    pub device_name: String,
    pub last_seen: String,
    pub current: bool,
}

/// Settings: panel address (+QR), Minecraft address and paired phones.
#[derive(Template, Debug)]
#[template(path = "devices.html")]
pub struct SettingsBody<'a> {
    pub devices: Vec<DeviceRow>,
    pub csrf_token: &'a str,
    pub panel_url: String,
    pub ip_url: Option<String>,
    pub qr_svg: Option<String>,
    pub mc_local: Option<String>,
    pub mc_ip: Option<String>,
}

// ----------------------------------------------------------------- helpers

/// `host` or `host:port` (the default Minecraft port is left out).
pub fn mc_address(host: &str, port: u16, default_port: u16) -> String {
    if port == default_port {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

/// `http://host[:port]/` for the panel.
pub fn panel_url(host: &str, port: u16) -> String {
    if port == 80 {
        format!("http://{host}/")
    } else {
        format!("http://{host}:{port}/")
    }
}

pub fn backup_kind_label(kind: &str) -> &'static str {
    match kind {
        "daily" => s::BACKUP_DAILY,
        "pre_change" => s::BACKUP_PRE_CHANGE,
        "pre_restore" => s::BACKUP_PRE_RESTORE,
        _ => s::BACKUP_MANUAL,
    }
}

/// `5.7 GiB`, `512 MiB`, `3 KiB`.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        let tenths = bytes.saturating_mul(10) / GIB;
        format!("{}.{} GiB", tenths / 10, tenths % 10)
    } else if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{} KiB", bytes / KIB)
    }
}

/// Memory shown on the create screen.
pub fn format_mib(mib: u64) -> String {
    format_bytes(mib.saturating_mul(MIB))
}

/// A backup id is `<server id>-<unix time>-<reason>`: letters, digits, `_`, `-`.
pub fn valid_backup_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Removes control characters (and bidi overrides) and caps the length. Used
/// for text that came from outside: log lines, failure reasons, server replies.
pub fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .filter(|c| !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(MAX_LOG_LINE_CHARS)
        .collect()
}

/// A log line ready for display: ANSI colour codes dropped, control characters
/// removed, length capped. HTML escaping happens in the template.
pub fn clean_log_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len().min(MAX_LOG_LINE_CHARS * 4));
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // A CSI sequence ends at a final byte in 0x40..=0x7e.
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    clean_text(&out)
}

/// A server reply shortened for display.
pub fn clean_reply(reply: &str) -> String {
    clean_text(reply).chars().take(MAX_REPLY_CHARS).collect()
}

/// Names out of the reply to `whitelist list`
/// (`There are 2 whitelisted player(s): Alice, Bob`). Anything that is not a
/// valid player name is dropped, so nothing odd can reach a page or a command.
pub fn parse_whitelist(reply: &str) -> Vec<String> {
    let Some((_, names)) = reply.split_once(':') else {
        return Vec::new();
    };
    let mut out: Vec<String> = names
        .split(',')
        .map(str::trim)
        .filter(|n| player_name(n).is_ok())
        .map(str::to_owned)
        .take(MAX_WHITELIST_SHOWN)
        .collect();
    out.sort_by_key(|n| n.to_ascii_lowercase());
    out.dedup();
    out
}

/// An inline SVG QR code for `text`, or `None` when it does not fit.
pub fn qr_svg(text: &str) -> Option<String> {
    use qrcodegen::{QrCode, QrCodeEcc};
    const QUIET: i32 = 4;
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let size = qr.size().checked_add(QUIET * 2)?;
    let mut path = String::new();
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                let _ = write!(path, "M{},{}h1v1h-1z", x + QUIET, y + QUIET);
            }
        }
    }
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"qr\" viewBox=\"0 0 {size} {size}\" \
         role=\"img\" aria-label=\"{label}\" shape-rendering=\"crispEdges\">\
         <rect width=\"{size}\" height=\"{size}\" fill=\"#ffffff\"/>\
         <path d=\"{path}\" fill=\"#000000\"/></svg>",
        label = s::SETTINGS_QR_LABEL,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_human() {
        assert_eq!(format_bytes(512), "0 KiB");
        assert_eq!(format_bytes(3 * 1024), "3 KiB");
        assert_eq!(format_bytes(63 * MIB), "63 MiB");
        assert_eq!(format_bytes(5 * 1024 * MIB + 700 * MIB), "5.6 GiB");
        assert_eq!(format_mib(5888), "5.7 GiB");
    }

    #[test]
    fn addresses() {
        assert_eq!(mc_address("a.local", 25565, 25565), "a.local");
        assert_eq!(mc_address("a.local", 25566, 25565), "a.local:25566");
        assert_eq!(panel_url("a.local", 8080), "http://a.local:8080/");
        assert_eq!(panel_url("a.local", 80), "http://a.local/");
    }

    #[test]
    fn log_lines_lose_escapes_and_controls() {
        assert_eq!(clean_log_line("\u{1b}[32mDone\u{1b}[0m (3s)"), "Done (3s)");
        assert_eq!(clean_log_line("a\r\u{0}b\u{7}c"), "abc");
        assert_eq!(clean_log_line("x\u{202e}y"), "xy");
        assert_eq!(
            clean_log_line(&"z".repeat(1000)).chars().count(),
            MAX_LOG_LINE_CHARS
        );
        // HTML stays as text here; the template escapes it.
        assert_eq!(clean_log_line("<script>"), "<script>");
        // An unterminated escape swallows the rest of the line, never panics.
        assert_eq!(clean_log_line("ok\u{1b}[31"), "ok");
        assert_eq!(clean_log_line("\u{1b}"), "");
    }

    #[test]
    fn whitelist_replies_parse() {
        assert_eq!(
            parse_whitelist("There are 3 whitelisted player(s): Zed, alice, Bob_1"),
            vec!["alice", "Bob_1", "Zed"]
        );
        assert!(parse_whitelist("There are no whitelisted players").is_empty());
        assert_eq!(
            parse_whitelist("There are 2 whitelisted players: ok_name, <b>x</b>, ab, a b"),
            vec!["ok_name"]
        );
        assert!(parse_whitelist("").is_empty());
    }

    #[test]
    fn backup_ids() {
        assert!(valid_backup_id("s1234abcd-1700000000-manual"));
        assert!(!valid_backup_id(""));
        assert!(!valid_backup_id("../x"));
        assert!(!valid_backup_id("a b"));
        assert!(!valid_backup_id(&"a".repeat(97)));
    }

    #[test]
    fn qr_is_a_self_contained_svg() {
        let svg = qr_svg("http://192.168.1.20:8080/").unwrap_or_default();
        assert!(svg.starts_with("<svg "));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains("<path d=\"M"));
        // Nothing the CSP would block: no style attribute, script or external href.
        for banned in ["style=", "<script", "href=", "http://192"] {
            assert!(!svg.contains(banned), "{banned}");
        }
    }

    #[test]
    fn kinds_have_labels() {
        assert_eq!(backup_kind_label("daily"), s::BACKUP_DAILY);
        assert_eq!(backup_kind_label("whatever"), s::BACKUP_MANUAL);
    }
}
