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
    /// The tunnel's public address, when online play is running.
    pub online_address: Option<String>,
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
    /// The newest releases, then older ones and snapshots in folded lists;
    /// the picked one is among them.
    pub releases: Vec<String>,
    pub older: Vec<String>,
    pub snapshots: Vec<String>,
    /// The chosen version is an older release, so that list starts open.
    pub older_picked: bool,
    /// Only the built-in list could be offered.
    pub short_list: bool,
    /// The chosen version is a snapshot, so that list starts open.
    pub snapshot_picked: bool,
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

#[derive(Debug)]
pub struct ModHitView {
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub author: String,
    pub downloads: String,
    pub side_label: &'static str,
    pub installed: bool,
    /// Where the browser loads the icon from: this daemon, never the CDN.
    pub icon: Option<String>,
    pub categories: Vec<String>,
}

impl ModHitView {
    pub fn new(hit: &crate::lifecycle::ModBrowseHit) -> ModHitView {
        let h = &hit.info;
        ModHitView {
            project_id: h.project_id.clone(),
            title: h.title.clone(),
            description: h.description.clone(),
            author: h.author.clone(),
            downloads: format!("{} downloads", format_count(h.downloads)),
            side_label: match h.server_side.as_str() {
                "required" => s::MODS_SIDE_REQUIRED,
                "optional" => s::MODS_SIDE_OPTIONAL,
                _ => s::MODS_SIDE_UNKNOWN,
            },
            installed: h.installed,
            icon: hit.icon_url.as_deref().and_then(icon_src),
            categories: hit
                .categories
                .iter()
                .take(3)
                .map(|c| category_label(c))
                .collect(),
        }
    }
}

/// The panel's own address for a CDN icon.
fn icon_src(cdn_url: &str) -> Option<String> {
    let query = serde_urlencoded::to_string([("u", cdn_url)]).ok()?;
    Some(format!("/mods/icon?{query}"))
}

/// `game-mechanics` -> `Game mechanics`.
pub fn category_label(c: &str) -> String {
    let mut out = c.replace('-', " ");
    if let Some(first) = out.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    out
}

/// A filter or ordering link on the mod browser.
#[derive(Debug)]
pub struct ChipView {
    pub label: String,
    pub href: String,
    pub active: bool,
}

/// `1.2M`, `34k`, `567`.
pub fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}.{}M", n / 1_000_000, n % 1_000_000 / 100_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// The Mods screen: search box, results and the installed list.
#[derive(Template, Debug)]
#[template(path = "mods.html")]
pub struct ModsBody<'a> {
    pub has_server: bool,
    pub busy: bool,
    pub query: &'a str,
    pub include_unknown: bool,
    pub results: Option<Vec<ModHitView>>,
    pub search_error: Option<String>,
    pub category: &'a str,
    pub sort: &'a str,
    pub categories: Vec<ChipView>,
    pub sorts: Vec<ChipView>,
    /// `Showing 1-20 of 345`.
    pub total_text: Option<String>,
    /// This same page again, for the retry link.
    pub retry: String,
    /// Some search text or switch is set, so "clear" makes sense.
    pub filtered: bool,
    pub installed_count: usize,
    pub prev: Option<String>,
    pub next: Option<String>,
    pub list: &'a str,
}

#[derive(Debug)]
pub struct InstalledView {
    pub project_id: String,
    pub title: String,
    pub version: String,
    pub dependency: bool,
    pub status: Option<&'static str>,
    pub required_by: String,
}

impl InstalledView {
    pub fn new(m: &craft_a_server_core::ctl::InstalledModInfo) -> InstalledView {
        InstalledView {
            project_id: m.project_id.clone(),
            title: m.title.clone(),
            version: m.version_number.clone(),
            dependency: m.as_dependency,
            status: match m.status.as_str() {
                "missing" => Some(s::MODS_STATUS_MISSING),
                "size_changed" => Some(s::MODS_STATUS_CHANGED),
                _ => None,
            },
            required_by: m.required_by.join(", "),
        }
    }
}

/// The installed list with the busy/result banner (also the htmx poll target).
#[derive(Template, Debug)]
#[template(path = "mods_list.html")]
pub struct ModsList {
    pub mods: Vec<InstalledView>,
    pub untracked: Vec<String>,
    pub banner: Banner,
}

#[derive(Template, Debug)]
#[template(path = "mods_install.html")]
pub struct ModInstallBody<'a> {
    pub title: &'a str,
    pub plan: craft_a_server_core::ctl::ModInstallPlanInfo,
    pub size: String,
    pub already: String,
    pub optional: String,
    pub action: String,
    pub prerelease: bool,
    pub csrf_token: &'a str,
}

#[derive(Template, Debug)]
#[template(path = "mods_remove.html")]
pub struct ModRemoveBody<'a> {
    pub plan: craft_a_server_core::ctl::ModRemovePlanInfo,
    pub dependents: String,
    pub orphans: String,
    pub action: String,
    pub csrf_token: &'a str,
}

#[derive(Template, Debug)]
#[template(path = "mods_updates.html")]
pub struct ModUpdatesBody {
    pub updates: Vec<craft_a_server_core::ctl::ModUpdateInfo>,
    pub error: Option<String>,
    pub busy: bool,
}

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

/// The Online play screen: a heading and the polled status partial.
#[derive(Template, Debug)]
#[template(path = "online.html")]
pub struct OnlineBody<'a> {
    pub status: &'a str,
}

/// What the status partial shows. Everything is plain text and escaped by
/// the template; the claim link has already been validated by the core.
#[derive(Template, Debug)]
#[template(path = "online_status.html")]
pub struct OnlineStatusBody<'a> {
    pub available: bool,
    pub enabled: bool,
    /// `off`, `preparing`, `claiming`, `claimed`, `running` or `error`.
    pub phase: &'static str,
    pub label: &'static str,
    pub claim_url: Option<String>,
    pub claim_code: Option<String>,
    pub address: Option<String>,
    pub error: Option<String>,
    /// Off, but a link to playit.gg is kept.
    pub keeps_link: bool,
    pub local_port: u16,
    pub agent_version: String,
    pub csrf_token: &'a str,
}

impl OnlineStatusBody<'_> {
    /// The partial for a daemon without online play.
    pub fn unavailable(csrf_token: &str) -> OnlineStatusBody<'_> {
        OnlineStatusBody {
            available: false,
            enabled: false,
            phase: "off",
            label: s::ONLINE_STATE_OFF,
            claim_url: None,
            claim_code: None,
            address: None,
            error: None,
            keeps_link: false,
            local_port: 0,
            agent_version: String::new(),
            csrf_token,
        }
    }
}

/// The disclosure and the confirm button.
#[derive(Template, Debug)]
#[template(path = "online_enable.html")]
pub struct OnlineEnableBody<'a> {
    pub csrf_token: &'a str,
}

/// Prepares the status partial from the tunnel's info.
pub fn online_status<'a>(
    info: &craft_a_server_core::tunnel::TunnelInfo,
    csrf_token: &'a str,
) -> OnlineStatusBody<'a> {
    use craft_a_server_core::tunnel::TunnelState;
    let (phase, label) = match (&info.state, info.enabled) {
        (TunnelState::Running { .. }, true) => ("running", s::ONLINE_STATE_RUNNING),
        (TunnelState::Error { .. }, _) => ("error", s::ONLINE_STATE_ERROR),
        (_, false) => ("off", s::ONLINE_STATE_OFF),
        (TunnelState::NotConfigured, true) => ("preparing", s::ONLINE_STATE_PREPARING),
        (TunnelState::Claiming { .. }, true) => ("claiming", s::ONLINE_STATE_CLAIMING),
        (TunnelState::Claimed, true) => ("claimed", s::ONLINE_STATE_CLAIMED),
    };
    let (mut claim_url, mut claim_code, mut address, mut error) = (None, None, None, None);
    match &info.state {
        TunnelState::Claiming { code, url } if info.enabled => {
            claim_url = Some(url.clone());
            claim_code = Some(code.clone());
        }
        TunnelState::Running { address: a } if info.enabled => address = Some(a.clone()),
        TunnelState::Error { reason } => error = Some(clean_text(reason)),
        _ => {}
    }
    OnlineStatusBody {
        available: true,
        enabled: info.enabled,
        phase,
        label,
        claim_url,
        claim_code,
        address,
        error,
        keeps_link: !info.enabled && info.state == TunnelState::Claimed,
        local_port: info.local_port,
        agent_version: info.agent_version.clone(),
        csrf_token,
    }
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

/// The World screen.
#[derive(Template, Debug)]
#[template(path = "world.html")]
pub struct WorldBody<'a> {
    pub status: &'a str,
    pub has_server: bool,
    pub pending: bool,
    pub busy: bool,
    pub csrf_token: &'a str,
    pub max_bytes: u64,
    pub max_text: String,
}

/// The busy line and last result of the World screen (also its poll target).
#[derive(Template, Debug)]
#[template(path = "world_status.html")]
pub struct WorldStatus {
    pub banner: Banner,
}

/// What an uploaded file would change, shown before anything is replaced.
#[derive(Template, Debug)]
#[template(path = "world_review.html")]
pub struct WorldReviewBody<'a> {
    pub csrf_token: &'a str,
    pub world_files: u64,
    pub world_size: String,
    pub mods: Vec<String>,
    pub mods_more: usize,
    pub replaces_mods: bool,
    pub config_files: u64,
    pub lists: String,
    pub icon: bool,
    pub properties: bool,
    pub ignored: u64,
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
    fn counts_are_short() {
        assert_eq!(format_count(567), "567");
        assert_eq!(format_count(34_500), "34k");
        assert_eq!(format_count(294_620_270), "294.6M");
    }

    #[test]
    fn kinds_have_labels() {
        assert_eq!(backup_kind_label("daily"), s::BACKUP_DAILY);
        assert_eq!(backup_kind_label("whatever"), s::BACKUP_MANUAL);
    }
}
