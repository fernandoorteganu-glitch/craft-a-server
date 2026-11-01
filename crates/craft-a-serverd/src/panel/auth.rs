//! Pairing codes, sessions, cookies and the [`AuthSession`] extractor.
//!
//! # Accepted risk: no `Secure` cookie flag
//!
//! The panel speaks plain HTTP on the home LAN (there is no certificate a
//! phone would trust for `craft-a-server.local`), so the session cookie cannot
//! carry `Secure`; browsers would drop it. A passive eavesdropper on the same
//! Wi-Fi could therefore read the cookie. This is mitigated, not eliminated,
//! by: the LAN-only peer filter, `HttpOnly` and `SameSite=Strict`, per-device
//! revocation, 30-day sliding expiry, and Wi-Fi encryption. Documented in the
//! threat model; revisit when the panel gains an HTTPS option.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use rand::TryRng;
use rand::rngs::SysRng;

use super::error::PanelError;
use super::strings as s;
use crate::db::{Db, SessionRow, hash_secret};

pub const COOKIE_NAME: &str = "craft_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
pub const CSRF_FIELD: &str = "csrf_token";

/// Pairing-code alphabet: 32 symbols (digits 2-9 and letters except `I` and
/// `O`), so one random byte masked with 31 picks a symbol with no modulo bias,
/// and the lookalikes `0/O` and `1/I` cannot be confused when typed from a
/// screen. 8 symbols give 40 bits, 10 minutes of validity and a rate limit.
pub const CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
pub const CODE_LEN: usize = 8;
pub const MAX_DEVICE_NAME_CHARS: usize = 40;
/// The session row is rewritten at most this often when a phone is in use.
pub const TOUCH_INTERVAL_SECS: i64 = 300;

/// What a handler knows about the signed-in phone. Never the token itself.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthSession {
    /// Public id (safe to show, log and use in URLs).
    pub id: String,
    pub device_name: String,
    /// Per-session CSRF token; templates embed it via the layout.
    pub csrf_token: String,
}

impl std::fmt::Debug for AuthSession {
    // The CSRF token is a secret: keep it out of logs and panic messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSession")
            .field("id", &self.id)
            .field("device_name", &self.device_name)
            .field("csrf_token", &"<redacted>")
            .finish()
    }
}

/// Extractor for handlers on protected routes. The guard middleware has
/// already authenticated the request and (for state-changing methods) checked
/// CSRF; this just reads the result. On a public route it answers 401.
impl<S: Send + Sync> FromRequestParts<S> for AuthSession {
    type Rejection = PanelError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthSession>()
            .cloned()
            .ok_or(PanelError::Unauthorized)
    }
}

/// `n` bytes from the operating system's CSPRNG.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], PanelError> {
    let mut buf = [0u8; N];
    SysRng
        .try_fill_bytes(&mut buf)
        .map_err(|e| PanelError::internal(&format_args!("system randomness unavailable: {e}")))?;
    Ok(buf)
}

pub fn random_hex<const N: usize>() -> Result<String, PanelError> {
    Ok(hex::encode(random_bytes::<N>()?))
}

/// A fresh pairing code (raw, 8 symbols, no dash).
pub fn generate_code() -> Result<String, PanelError> {
    let bytes = random_bytes::<CODE_LEN>()?;
    Ok(bytes
        .iter()
        .map(|b| {
            let symbol = CODE_ALPHABET.get(usize::from(b & 31)).copied();
            char::from(symbol.unwrap_or(b'X'))
        })
        .collect())
}

/// `ABCD2345` -> `ABCD-2345`.
pub fn display_code(code: &str) -> String {
    match (code.get(..4), code.get(4..)) {
        (Some(a), Some(b)) if !b.is_empty() => format!("{a}-{b}"),
        _ => code.to_owned(),
    }
}

/// What the user typed -> what gets hashed: uppercase, without spaces and dashes.
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '\u{2010}'..='\u{2015}'))
        .flat_map(char::to_uppercase)
        .take(64)
        .collect()
}

/// A code just created for the owner to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCode {
    /// Raw 8 symbols.
    pub code: String,
    pub expires_unix: i64,
}

/// Creates and stores a pairing code (only its hash reaches the database).
pub fn issue_pairing_code(db: &Db, now: i64, ttl_secs: u64) -> Result<IssuedCode, PanelError> {
    let code = generate_code()?;
    let expires_unix = now.saturating_add(i64::try_from(ttl_secs).unwrap_or(i64::MAX / 2));
    db.purge_expired_pairing_codes(now)?;
    db.insert_pairing_code(&hash_secret(&code), now, expires_unix)?;
    Ok(IssuedCode { code, expires_unix })
}

/// First-run helper: a code only when nobody is paired and no code is pending.
pub fn first_run_code(db: &Db, now: i64, ttl_secs: u64) -> Result<Option<IssuedCode>, PanelError> {
    if db.list_sessions(now)?.is_empty() && db.count_pairing_codes(now)? == 0 {
        Ok(Some(issue_pairing_code(db, now, ttl_secs)?))
    } else {
        Ok(None)
    }
}

/// Cleans a user-supplied device name: control and bidi-override characters
/// removed, trimmed, at most 40 characters. `None` when nothing is left.
pub fn clean_device_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c,
                    '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
        })
        .collect();
    let trimmed: String = cleaned
        .trim()
        .chars()
        .take(MAX_DEVICE_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// A default device name from the `User-Agent` header.
pub fn device_name_from_user_agent(ua: Option<&str>) -> &'static str {
    let ua = ua.unwrap_or_default();
    if ua.contains("iPhone") {
        s::DEVICE_IPHONE
    } else if ua.contains("iPad") {
        s::DEVICE_IPAD
    } else if ua.contains("Android") {
        if ua.contains("Mobile") {
            s::DEVICE_ANDROID
        } else {
            s::DEVICE_ANDROID_TABLET
        }
    } else {
        s::DEVICE_BROWSER
    }
}

/// Constant-time equality for secrets of any length (length is not secret
/// here: tokens have a fixed public length).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = u8::from(a.len() != b.len());
    for (i, x) in a.iter().enumerate() {
        diff |= x ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// A new session for a just-redeemed pairing code. Returns the row (hashes
/// only) and the raw cookie token, which exists nowhere else.
pub fn new_session(
    device_name: &str,
    now: i64,
    ttl_secs: u64,
) -> Result<(SessionRow, String), PanelError> {
    let token = random_hex::<32>()?;
    let row = SessionRow {
        id: random_hex::<8>()?,
        token_hash: hash_secret(&token),
        csrf_token: random_hex::<32>()?,
        device_name: device_name.to_owned(),
        created_unix: now,
        last_seen_unix: now,
        expires_unix: now.saturating_add(i64::try_from(ttl_secs).unwrap_or(i64::MAX / 2)),
    };
    Ok((row, token))
}

/// `Set-Cookie` value. No `Secure` (see the module docs).
pub fn session_cookie(token: &str, max_age_secs: u64) -> String {
    format!("{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age_secs}")
}

/// `Set-Cookie` value that removes the cookie.
pub fn clear_cookie() -> String {
    session_cookie("", 0)
}

/// Is this a well-formed session token (64 lowercase hex chars)?
fn token_shaped(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The session token from a `Cookie` header, if there is exactly one well
/// formed `craft_session` cookie.
pub fn token_from_cookie_header(header: &str) -> Option<&str> {
    let mut found = None;
    for part in header.split(';') {
        if let Some((name, value)) = part.trim().split_once('=')
            && name == COOKIE_NAME
        {
            if found.is_some() {
                return None;
            }
            found = Some(value);
        }
    }
    found.filter(|t| token_shaped(t))
}

/// Pulls the CSRF field out of an urlencoded form body without decoding
/// anything else (the token is hex, so it never needs unescaping).
pub fn csrf_from_form(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == CSRF_FIELD).then(|| v.to_owned())
    })
}

/// Outcome of a successful sign-in.
#[derive(Debug)]
pub struct PairOutcome {
    pub token: String,
    pub session: SessionRow,
}

/// Redeems `typed_code` and, when valid, stores a new session.
///
/// The code is normalised and hashed, and the database consulted, the same way
/// whether or not the input looks plausible, so timing does not reveal
/// anything about near misses. Blocking: call from `spawn_blocking`.
pub fn redeem_code(
    db: &Db,
    typed_code: &str,
    device_name: &str,
    now: i64,
    session_ttl_secs: u64,
) -> Result<Option<PairOutcome>, PanelError> {
    let hash = hash_secret(&normalize_code(typed_code));
    let valid = db.take_pairing_code(&hash, now)?;
    if !valid {
        return Ok(None);
    }
    let (session, token) = new_session(device_name, now, session_ttl_secs)?;
    db.insert_session(&session)?;
    Ok(Some(PairOutcome { token, session }))
}

/// Shared handle type used by middleware.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_is_32_unambiguous_symbols() {
        assert_eq!(CODE_ALPHABET.len(), 32);
        for banned in *b"01IO" {
            assert!(!CODE_ALPHABET.contains(&banned));
        }
        let mut sorted = CODE_ALPHABET.to_vec();
        sorted.dedup();
        assert_eq!(sorted.len(), 32);
    }

    #[test]
    fn generated_codes_are_well_formed_and_vary() -> Result<(), PanelError> {
        let a = generate_code()?;
        let b = generate_code()?;
        assert_eq!(a.len(), CODE_LEN);
        assert!(a.bytes().all(|c| CODE_ALPHABET.contains(&c)));
        assert_ne!(a, b);
        Ok(())
    }

    #[test]
    fn normalisation_and_display() {
        for typed in [
            "abcd-2345",
            "ABCD 2345",
            " abcd2345 ",
            "a-b-c-d-2-3-4-5",
            "AbCd\u{2010}2345",
        ] {
            assert_eq!(normalize_code(typed), "ABCD2345", "{typed:?}");
        }
        assert_eq!(display_code("ABCD2345"), "ABCD-2345");
        assert_eq!(display_code("AB"), "AB");
        assert!(normalize_code(&"a".repeat(1000)).len() <= 64);
    }

    #[test]
    fn device_names() {
        assert_eq!(
            clean_device_name("  Mum's Pixel  "),
            Some("Mum's Pixel".into())
        );
        assert_eq!(
            clean_device_name("a\u{0}b\nc\u{202E}d"),
            Some("abcd".into())
        );
        assert_eq!(clean_device_name("   "), None);
        assert_eq!(clean_device_name("\u{7}\u{1b}"), None);
        let long = clean_device_name(&"é".repeat(100)).unwrap_or_default();
        assert_eq!(long.chars().count(), 40);
        assert_eq!(
            clean_device_name(&format!("{} x", "a".repeat(39))),
            Some("a".repeat(39))
        );
    }

    #[test]
    fn user_agent_defaults() {
        let table = [
            (
                Some("Mozilla/5.0 (Linux; Android 14; Pixel 8) Mobile Safari/537"),
                "Android phone",
            ),
            (
                Some("Mozilla/5.0 (Linux; Android 14; SM-X900) Safari/537"),
                "Android tablet",
            ),
            (
                Some("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)"),
                "iPhone",
            ),
            (
                Some("Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)"),
                "iPad",
            ),
            (Some("Mozilla/5.0 (Windows NT 10.0) Firefox/130"), "Browser"),
            (Some(""), "Browser"),
            (None, "Browser"),
        ];
        for (ua, want) in table {
            assert_eq!(device_name_from_user_agent(ua), want, "{ua:?}");
        }
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("abcd", "abc"));
        assert!(!ct_eq("", "a"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn cookie_attributes_are_exact() {
        assert_eq!(
            session_cookie("tok", 2_592_000),
            "craft_session=tok; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"
        );
        assert_eq!(
            clear_cookie(),
            "craft_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
        );
        assert!(!session_cookie("t", 1).contains("Secure"));
    }

    #[test]
    fn cookie_parsing() {
        let t = "a".repeat(64);
        let ok = |h: &str| token_from_cookie_header(h).map(str::to_owned);
        assert_eq!(ok(&format!("craft_session={t}")), Some(t.clone()));
        assert_eq!(ok(&format!("x=1; craft_session={t}; y=2")), Some(t.clone()));
        // Duplicates, wrong shape and look-alike names are ignored.
        assert_eq!(ok(&format!("craft_session={t}; craft_session={t}")), None);
        assert_eq!(ok("craft_session=short"), None);
        assert_eq!(ok(&format!("craft_session={}", "A".repeat(64))), None);
        assert_eq!(ok(&format!("xcraft_session={t}")), None);
        assert_eq!(ok(&format!("craft_session ={t}")), None);
        assert_eq!(ok(""), None);
        assert_eq!(ok("craft_session"), None);
    }

    #[test]
    fn csrf_form_field() {
        assert_eq!(
            csrf_from_form(b"a=1&csrf_token=abc&b=2"),
            Some("abc".into())
        );
        assert_eq!(csrf_from_form(b"csrf_token=abc"), Some("abc".into()));
        assert_eq!(csrf_from_form(b"xcsrf_token=abc"), None);
        assert_eq!(csrf_from_form(b"a=1"), None);
        assert_eq!(csrf_from_form(&[0xff, 0xfe]), None);
    }

    #[test]
    fn codes_are_redeemed_once_and_sessions_store_only_hashes() -> Result<(), PanelError> {
        let db = Db::open_in_memory()?;
        let issued = issue_pairing_code(&db, 1000, 600)?;
        assert_eq!(issued.expires_unix, 1600);
        // The raw code is nowhere in the database.
        let typed = display_code(&issued.code).to_lowercase();
        let out = redeem_code(&db, &typed, "Pixel", 1001, 3600)?.ok_or(PanelError::Internal)?;
        assert_eq!(out.session.token_hash, hash_secret(&out.token));
        assert_ne!(out.session.token_hash, out.token);
        assert_eq!(out.token.len(), 64);
        assert!(redeem_code(&db, &typed, "Pixel", 1002, 3600)?.is_none());
        assert!(db.find_session(&hash_secret(&out.token), 1003)?.is_some());
        Ok(())
    }

    #[test]
    fn expired_and_wrong_codes_fail_alike() -> Result<(), PanelError> {
        let db = Db::open_in_memory()?;
        let issued = issue_pairing_code(&db, 0, 600)?;
        assert!(redeem_code(&db, &issued.code, "x", 600, 60)?.is_none());
        assert!(redeem_code(&db, "AAAAAAAA", "x", 1, 60)?.is_none());
        assert!(redeem_code(&db, "", "x", 1, 60)?.is_none());
        Ok(())
    }

    #[test]
    fn first_run_code_only_when_nothing_exists() -> Result<(), PanelError> {
        let db = Db::open_in_memory()?;
        assert!(first_run_code(&db, 0, 600)?.is_some());
        // A valid code is pending now.
        assert!(first_run_code(&db, 1, 600)?.is_none());
        // Once expired and nobody paired, a new one is made.
        assert!(first_run_code(&db, 700, 600)?.is_some());
        let code = issue_pairing_code(&db, 800, 600)?;
        redeem_code(&db, &code.code, "x", 801, 3600)?.ok_or(PanelError::Internal)?;
        assert!(first_run_code(&db, 802, 600)?.is_none());
        assert!(first_run_code(&db, 100_000, 600)?.is_some());
        Ok(())
    }
}
