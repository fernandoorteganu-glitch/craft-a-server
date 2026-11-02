//! Tunnel types shared by the daemon, the CLI and the panel (playit.gg).
//!
//! The tunnel is optional and off by default. Its life cycle is a small state
//! machine that is persisted by the daemon and driven by [`next`], a pure
//! function with no I/O:
//!
//! ```text
//! NotConfigured -> Claiming(code, url) -> Claimed -> Running(address)
//!        \______________________\______________\_____> Error(reason)
//! ```
//!
//! Everything that reaches a screen from outside (the claim link, the public
//! address) is validated by this module before it becomes a state, so a
//! hostile or garbled answer from the agent or the playit API cannot put
//! arbitrary text in front of the user.

use serde::{Deserialize, Serialize};

use crate::config::is_plain_hostname;

/// The only place a claim link may point to.
pub const CLAIM_URL_PREFIX: &str = "https://playit.gg/claim/";

/// Port players use when the address carries none.
pub const DEFAULT_JAVA_PORT: u16 = 25565;

/// What enabling the tunnel shares with playit.gg. Shown before the user
/// confirms, in the CLI and in the panel.
pub const DISCLOSURE_TRAFFIC: &str = "While online play is on, all traffic between your friends and your \
     Minecraft server passes through playit.gg, a third-party service run by a company in the USA.";
pub const DISCLOSURE_IPS: &str = "playit.gg can see every player's IP address, how much data they send \
     and when they connect. Chat and gameplay are not encrypted by Minecraft Java on the way.";
pub const DISCLOSURE_POLICY: &str = "Its privacy policy is short and old (2020) and does not mention \
     GDPR. Your use is governed by its terms of service (Nevada, USA law, no uptime guarantee).";
pub const DISCLOSURE_ACCOUNT: &str = "You will be asked to link this machine to a playit.gg account \
     (a guest account works) in your browser. Your home network stays closed: the connection \
     goes out from this machine, nothing is opened on your router.";
pub const DISCLOSURE_OFF: &str = "You can turn it off at any time. Players on your home network \
     keep joining directly and are not affected.";

/// The disclosure paragraphs, in reading order.
pub fn disclosure() -> [&'static str; 5] {
    [
        DISCLOSURE_TRAFFIC,
        DISCLOSURE_IPS,
        DISCLOSURE_POLICY,
        DISCLOSURE_ACCOUNT,
        DISCLOSURE_OFF,
    ]
}

/// Longest failure reason kept in the state.
pub const MAX_REASON_CHARS: usize = 300;

/// Observable state of the tunnel, shown in the panel and the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TunnelState {
    /// No claim exists (never set up, or reset).
    NotConfigured,
    /// Waiting for the user to open the link and accept in a browser.
    Claiming { code: String, url: String },
    /// The agent is linked to an account; no public address known yet.
    Claimed,
    /// The agent is connected and the public address is known.
    Running { address: String },
    /// Something is wrong; the agent is retried with backoff.
    Error { reason: String },
}

impl TunnelState {
    /// Stable name used in the database and in logs.
    pub fn name(&self) -> &'static str {
        match self {
            TunnelState::NotConfigured => "not_configured",
            TunnelState::Claiming { .. } => "claiming",
            TunnelState::Claimed => "claimed",
            TunnelState::Running { .. } => "running",
            TunnelState::Error { .. } => "error",
        }
    }

    /// Rebuilds a state from stored parts, re-validating what is shown to
    /// people. Returns `None` for unknown names or values that do not pass.
    pub fn from_parts(
        name: &str,
        claim_url: Option<&str>,
        address: Option<&str>,
        reason: Option<&str>,
    ) -> Option<TunnelState> {
        Some(match name {
            "not_configured" => TunnelState::NotConfigured,
            "claiming" => {
                let (code, url) = parse_claim_url(claim_url?)?;
                TunnelState::Claiming { code, url }
            }
            "claimed" => TunnelState::Claimed,
            "running" => {
                let address = address?;
                if !is_valid_public_address(address) {
                    return None;
                }
                TunnelState::Running {
                    address: address.to_owned(),
                }
            }
            "error" => TunnelState::Error {
                reason: clean_reason(reason.unwrap_or("unknown error")),
            },
            _ => return None,
        })
    }
}

/// Everything a client needs to show the tunnel. Never carries the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelInfo {
    /// The user turned online play on (after confirming the disclosure).
    pub enabled: bool,
    pub state: TunnelState,
    /// When the user confirmed the disclosure, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_unix: Option<i64>,
    pub updated_unix: i64,
    /// Version of the pinned agent this build installs.
    pub agent_version: String,
    /// The local port the playit tunnel must point at (`127.0.0.1:<port>`).
    pub local_port: u16,
}

/// Inputs of the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelEvent {
    /// The user enabled online play. `has_secret`: a claim already exists.
    Enabled { has_secret: bool },
    /// The user disabled online play.
    Disabled { has_secret: bool },
    /// The claim link is ready (validated with [`parse_claim_url`]).
    ClaimLinkReady { code: String, url: String },
    /// The user accepted the claim; the agent now holds its secret.
    SecretStored,
    /// The playit API reported a public address (validated).
    AddressKnown { address: String },
    /// The agent stopped or the tunnel disappeared; the claim stays.
    AddressLost,
    /// Something failed.
    Failed { reason: String },
    /// A retry is about to be made after a failure.
    Recovered { has_secret: bool },
}

/// The event does not make sense in the current state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("tunnel event {event} is not valid while {state}")]
pub struct TunnelTransitionError {
    pub state: &'static str,
    pub event: &'static str,
}

impl TunnelEvent {
    fn name(&self) -> &'static str {
        match self {
            TunnelEvent::Enabled { .. } => "enabled",
            TunnelEvent::Disabled { .. } => "disabled",
            TunnelEvent::ClaimLinkReady { .. } => "claim_link_ready",
            TunnelEvent::SecretStored => "secret_stored",
            TunnelEvent::AddressKnown { .. } => "address_known",
            TunnelEvent::AddressLost => "address_lost",
            TunnelEvent::Failed { .. } => "failed",
            TunnelEvent::Recovered { .. } => "recovered",
        }
    }
}

fn claimed_or_not(has_secret: bool) -> TunnelState {
    if has_secret {
        TunnelState::Claimed
    } else {
        TunnelState::NotConfigured
    }
}

/// The pure transition function. Content carried by events is validated
/// again here, so no caller can store an unchecked link or address.
pub fn next(
    state: &TunnelState,
    event: &TunnelEvent,
) -> Result<TunnelState, TunnelTransitionError> {
    use TunnelState as S;
    let bad = || TunnelTransitionError {
        state: state.name(),
        event: event.name(),
    };
    Ok(match (state, event) {
        // Disabling always works and always lands on a resting state.
        (_, TunnelEvent::Disabled { has_secret }) => claimed_or_not(*has_secret),
        // Failures can happen anywhere.
        (_, TunnelEvent::Failed { reason }) => S::Error {
            reason: clean_reason(reason),
        },

        // Enabling an active tunnel changes nothing.
        (S::Claiming { .. } | S::Running { .. }, TunnelEvent::Enabled { .. }) => state.clone(),
        (_, TunnelEvent::Enabled { has_secret }) => claimed_or_not(*has_secret),

        (S::Error { .. }, TunnelEvent::Recovered { has_secret }) => claimed_or_not(*has_secret),
        (_, TunnelEvent::Recovered { .. }) => state.clone(),

        (
            S::NotConfigured | S::Claiming { .. } | S::Error { .. },
            TunnelEvent::ClaimLinkReady { code, url },
        ) => {
            let (parsed_code, parsed_url) = parse_claim_url(url).ok_or_else(bad)?;
            if parsed_code != *code {
                return Err(bad());
            }
            S::Claiming {
                code: parsed_code,
                url: parsed_url,
            }
        }

        (
            S::NotConfigured | S::Claiming { .. } | S::Claimed | S::Error { .. },
            TunnelEvent::SecretStored,
        ) => S::Claimed,
        (S::Running { .. }, TunnelEvent::SecretStored) => state.clone(),

        (
            S::Claimed | S::Running { .. } | S::Error { .. },
            TunnelEvent::AddressKnown { address },
        ) => {
            if !is_valid_public_address(address) {
                return Err(bad());
            }
            S::Running {
                address: address.clone(),
            }
        }

        (S::Running { .. } | S::Claimed, TunnelEvent::AddressLost) => S::Claimed,

        _ => return Err(bad()),
    })
}

/// Keeps a failure reason short, single-line and printable.
pub fn clean_reason(reason: &str) -> String {
    let mut out: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_REASON_CHARS)
        .collect();
    out = out.trim().to_owned();
    if out.is_empty() {
        out.push_str("unknown error");
    }
    out
}

/// Validates a claim link and returns `(code, url)`.
///
/// Exactly `https://playit.gg/claim/<code>`: the code is 4 to 64 ASCII
/// letters or digits, no query, no fragment, nothing else.
pub fn parse_claim_url(url: &str) -> Option<(String, String)> {
    let code = url.strip_prefix(CLAIM_URL_PREFIX)?;
    if !(4..=64).contains(&code.len()) || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    Some((code.to_owned(), url.to_owned()))
}

/// Builds the public address text players type: `host`, or `host:port` when
/// the port is not Minecraft's default. Returns `None` unless the host is a
/// lower-case DNS name made of letters, digits, dots and inner hyphens with a
/// non-numeric last label (so no IP literals), and the port is 1..=65535.
pub fn validate_public_address(host: &str, port: u32) -> Option<String> {
    let port = u16::try_from(port).ok().filter(|p| *p != 0)?;
    if host.len() < 4
        || !host.contains('.')
        || !is_plain_hostname(host)
        || host.bytes().any(|b| b.is_ascii_uppercase())
    {
        return None;
    }
    let last = host.rsplit('.').next()?;
    if !last.bytes().any(|b| b.is_ascii_lowercase()) {
        return None;
    }
    Some(if port == DEFAULT_JAVA_PORT {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    })
}

/// True when `text` is something [`validate_public_address`] could return.
pub fn is_valid_public_address(text: &str) -> bool {
    match text.split_once(':') {
        None => {
            validate_public_address(text, u32::from(DEFAULT_JAVA_PORT)).as_deref() == Some(text)
        }
        Some((host, port)) => {
            let Ok(port) = port.parse::<u32>() else {
                return false;
            };
            port != u32::from(DEFAULT_JAVA_PORT)
                && validate_public_address(host, port).as_deref() == Some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const URL: &str = "https://playit.gg/claim/a1b2c3d4e5";

    fn claiming() -> TunnelState {
        TunnelState::Claiming {
            code: "a1b2c3d4e5".into(),
            url: URL.into(),
        }
    }

    fn running() -> TunnelState {
        TunnelState::Running {
            address: "mine.craft.playit.gg:12345".into(),
        }
    }

    fn error() -> TunnelState {
        TunnelState::Error {
            reason: "boom".into(),
        }
    }

    fn all_states() -> Vec<TunnelState> {
        vec![
            TunnelState::NotConfigured,
            claiming(),
            TunnelState::Claimed,
            running(),
            error(),
        ]
    }

    fn all_events() -> Vec<TunnelEvent> {
        vec![
            TunnelEvent::Enabled { has_secret: false },
            TunnelEvent::Enabled { has_secret: true },
            TunnelEvent::Disabled { has_secret: false },
            TunnelEvent::Disabled { has_secret: true },
            TunnelEvent::ClaimLinkReady {
                code: "a1b2c3d4e5".into(),
                url: URL.into(),
            },
            TunnelEvent::SecretStored,
            TunnelEvent::AddressKnown {
                address: "mine.craft.playit.gg".into(),
            },
            TunnelEvent::AddressLost,
            TunnelEvent::Failed {
                reason: "boom".into(),
            },
            TunnelEvent::Recovered { has_secret: true },
            TunnelEvent::Recovered { has_secret: false },
        ]
    }

    #[test]
    fn happy_path() -> TestResult {
        let s = next(
            &TunnelState::NotConfigured,
            &TunnelEvent::Enabled { has_secret: false },
        )?;
        assert_eq!(s, TunnelState::NotConfigured);
        let s = next(
            &s,
            &TunnelEvent::ClaimLinkReady {
                code: "a1b2c3d4e5".into(),
                url: URL.into(),
            },
        )?;
        assert_eq!(s, claiming());
        let s = next(&s, &TunnelEvent::SecretStored)?;
        assert_eq!(s, TunnelState::Claimed);
        let s = next(
            &s,
            &TunnelEvent::AddressKnown {
                address: "mine.craft.playit.gg:12345".into(),
            },
        )?;
        assert_eq!(s, running());
        let s = next(&s, &TunnelEvent::AddressLost)?;
        assert_eq!(s, TunnelState::Claimed);
        Ok(())
    }

    #[test]
    fn every_pair_is_decided_without_panicking() {
        // The table is total: each (state, event) is Ok or a typed error, and
        // an Ok result is always a state that survives a database round trip.
        for state in all_states() {
            for event in all_events() {
                if let Ok(out) = next(&state, &event) {
                    let (name, url, address, reason) = parts(&out);
                    assert_eq!(
                        TunnelState::from_parts(
                            name,
                            url.as_deref(),
                            address.as_deref(),
                            reason.as_deref()
                        ),
                        Some(out.clone()),
                        "{state:?} + {event:?}"
                    );
                }
            }
        }
    }

    fn parts(s: &TunnelState) -> (&'static str, Option<String>, Option<String>, Option<String>) {
        match s {
            TunnelState::Claiming { url, .. } => (s.name(), Some(url.clone()), None, None),
            TunnelState::Running { address } => (s.name(), None, Some(address.clone()), None),
            TunnelState::Error { reason } => (s.name(), None, None, Some(reason.clone())),
            _ => (s.name(), None, None, None),
        }
    }

    #[test]
    fn disable_and_failure_work_from_every_state() -> TestResult {
        for state in all_states() {
            assert_eq!(
                next(&state, &TunnelEvent::Disabled { has_secret: true })?,
                TunnelState::Claimed
            );
            assert_eq!(
                next(&state, &TunnelEvent::Disabled { has_secret: false })?,
                TunnelState::NotConfigured
            );
            assert!(matches!(
                next(&state, &TunnelEvent::Failed { reason: "x".into() })?,
                TunnelState::Error { .. }
            ));
        }
        Ok(())
    }

    #[test]
    fn enabling_keeps_an_active_tunnel() -> TestResult {
        for state in [claiming(), running()] {
            assert_eq!(
                next(&state, &TunnelEvent::Enabled { has_secret: true })?,
                state
            );
        }
        assert_eq!(
            next(&error(), &TunnelEvent::Enabled { has_secret: true })?,
            TunnelState::Claimed
        );
        Ok(())
    }

    #[test]
    fn an_address_needs_a_claim() {
        for state in [TunnelState::NotConfigured, claiming()] {
            assert!(
                next(
                    &state,
                    &TunnelEvent::AddressKnown {
                        address: "mine.craft.playit.gg".into()
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn the_machine_refuses_unchecked_content() {
        let bad_urls = [
            "https://evil.example/claim/a1b2c3d4e5",
            "http://playit.gg/claim/a1b2c3d4e5",
            "https://playit.gg/claim/a1b2c3d4e5?x=1",
            "https://playit.gg/claim/<script>",
            "https://playit.gg/claim/",
            "https://playit.gg/claim/ab",
        ];
        for url in bad_urls {
            assert!(
                next(
                    &TunnelState::NotConfigured,
                    &TunnelEvent::ClaimLinkReady {
                        code: "a1b2c3d4e5".into(),
                        url: url.into()
                    }
                )
                .is_err(),
                "{url}"
            );
        }
        // The code must be the tail of the URL.
        assert!(
            next(
                &TunnelState::NotConfigured,
                &TunnelEvent::ClaimLinkReady {
                    code: "other".into(),
                    url: URL.into()
                }
            )
            .is_err()
        );
        for address in ["", "evil host", "<b>.playit.gg", "10.0.0.1", "a.b:99999"] {
            assert!(
                next(
                    &TunnelState::Claimed,
                    &TunnelEvent::AddressKnown {
                        address: address.into()
                    }
                )
                .is_err(),
                "{address}"
            );
        }
    }

    #[test]
    fn public_address_validation() {
        assert_eq!(
            validate_public_address("mine.craft.playit.gg", 25565).as_deref(),
            Some("mine.craft.playit.gg")
        );
        assert_eq!(
            validate_public_address("mine.craft.playit.gg", 12345).as_deref(),
            Some("mine.craft.playit.gg:12345")
        );
        assert_eq!(
            validate_public_address("a-b.c1.gl.at.ply.gg", 1).as_deref(),
            Some("a-b.c1.gl.at.ply.gg:1")
        );
        for (host, port) in [
            ("mine.craft.playit.gg", 0),
            ("mine.craft.playit.gg", 65536),
            ("mine.craft.playit.gg", u32::MAX),
            ("MINE.craft.playit.gg", 25565),
            ("localhost", 25565),
            ("1.2.3.4", 25565),
            ("[::1]", 25565),
            ("mine.craft.playit.gg:80", 25565),
            ("mine craft.playit.gg", 25565),
            ("mine.craft.playit.gg/", 25565),
            ("-mine.craft.playit.gg", 25565),
            ("mine..playit.gg", 25565),
            ("mine.craft.playit.gg\n", 25565),
            ("m\u{e9}ne.playit.gg", 25565),
            ("", 25565),
        ] {
            assert_eq!(validate_public_address(host, port), None, "{host}:{port}");
        }
        let long = format!("{}.gg", "a".repeat(260));
        assert_eq!(validate_public_address(&long, 25565), None);
    }

    #[test]
    fn stored_addresses_are_revalidated() {
        assert!(is_valid_public_address("mine.craft.playit.gg"));
        assert!(is_valid_public_address("mine.craft.playit.gg:12345"));
        assert!(!is_valid_public_address("mine.craft.playit.gg:25565"));
        assert!(!is_valid_public_address("mine.craft.playit.gg:0"));
        assert!(!is_valid_public_address("mine.craft.playit.gg:+5"));
        assert!(!is_valid_public_address("mine.craft.playit.gg:012345"));
        assert!(!is_valid_public_address("<script>alert(1)</script>"));
        assert_eq!(
            TunnelState::from_parts("running", None, Some("<b>"), None),
            None
        );
        assert_eq!(TunnelState::from_parts("running", None, None, None), None);
        assert_eq!(
            TunnelState::from_parts("claiming", Some("https://evil/x"), None, None),
            None
        );
        assert_eq!(TunnelState::from_parts("nope", None, None, None), None);
    }

    #[test]
    fn reasons_are_single_line_and_bounded() {
        let r = clean_reason(&format!("a\nb\u{7}{}", "x".repeat(1000)));
        assert!(!r.contains('\n') && !r.contains('\u{7}'));
        assert!(r.chars().count() <= MAX_REASON_CHARS);
        assert_eq!(clean_reason("  \n "), "unknown error");
    }

    #[test]
    fn info_wire_format() -> TestResult {
        let info = TunnelInfo {
            enabled: true,
            state: running(),
            consent_unix: Some(5),
            updated_unix: 6,
            agent_version: "1.2.0".into(),
            local_port: 25565,
        };
        let json = serde_json::to_string(&info)?;
        assert!(json.contains(r#""state":"running""#), "{json}");
        assert_eq!(serde_json::from_str::<TunnelInfo>(&json)?, info);
        Ok(())
    }

    #[test]
    fn disclosure_names_the_third_party() {
        let text = disclosure().join(" ");
        assert!(text.contains("playit.gg"));
        assert!(text.contains("IP address"));
        assert!(text.contains("privacy policy"));
    }
}
