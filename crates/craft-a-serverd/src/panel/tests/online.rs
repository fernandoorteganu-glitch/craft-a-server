// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! Tests of the Online play screen: the confirm step before enabling, what is
//! shown in each state, escaping, CSRF and that nothing inline sneaks in.
//! The supervisor is not running here; the tests set the stored state and
//! look at the page.

use craft_a_server_core::tunnel::TunnelState;

use super::*;
use crate::db::TunnelRow;

fn fields(s: &Sess, rest: &str) -> String {
    format!("csrf_token={}&{rest}", s.csrf)
}

async fn get(h: &Harness, s: &Sess, path: &str) -> Resp {
    h.send(Rb::get(path).cookie(&s.token)).await
}

async fn post(h: &Harness, s: &Sess, path: &str, rest: &str) -> Resp {
    h.send(Rb::post(path).cookie(&s.token).form(&fields(s, rest)))
        .await
}

fn set_state(h: &Harness, enabled: bool, state: TunnelState) {
    h.db()
        .set_tunnel(&TunnelRow {
            enabled,
            state,
            consent_unix: Some(T0),
            updated_unix: T0,
        })
        .expect("set tunnel");
}

fn assert_no_inline_code(page: &str) {
    for needle in ["<script", " style=", " onclick=", " onload=", "javascript:"] {
        // The layout loads the vendored htmx with `<script src=...>`.
        let body = page.replace("<script src=\"/static/htmx.min.js\" defer></script>", "");
        assert!(!body.contains(needle), "found {needle} in {body}");
    }
}

#[tokio::test]
async fn the_screen_is_in_the_navigation_and_starts_off() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    let res = get(&h, &s, "/online").await;
    assert_eq!(res.status, StatusCode::OK);
    let page = res.text();
    assert!(
        page.contains("href=\"/online\" aria-current=\"page\""),
        "{page}"
    );
    assert!(page.contains("Online play"));
    assert!(page.contains(">Off<"), "{page}");
    assert!(page.contains("href=\"/online/enable\""));
    assert!(!page.contains("/online/disable"));
    assert_no_inline_code(&page);
    // Other screens link to it too.
    assert!(get(&h, &s, "/").await.text().contains("href=\"/online\""));
    Ok(())
}

#[tokio::test]
async fn enabling_shows_what_is_shared_and_needs_the_confirm_step() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    let confirm = get(&h, &s, "/online/enable").await;
    assert_eq!(confirm.status, StatusCode::OK);
    let page = confirm.text();
    for needle in [
        "playit.gg",
        "IP address",
        "third-party",
        "privacy policy",
        "name=\"confirm\" value=\"yes\"",
        "I understand, turn it on",
    ] {
        assert!(page.contains(needle), "missing {needle}: {page}");
    }
    assert_no_inline_code(&page);

    // The POST without the confirm field changes nothing.
    let refused = post(&h, &s, "/online/enable", "").await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{}",
        refused.text()
    );
    let wrong = post(&h, &s, "/online/enable", "confirm=no").await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    assert!(!h.db().get_tunnel()?.enabled);
    assert_eq!(h.db().get_tunnel()?.consent_unix, None);

    // Without the CSRF token it is refused before it gets that far.
    let no_csrf = h
        .send(
            Rb::post("/online/enable")
                .cookie(&s.token)
                .form("confirm=yes"),
        )
        .await;
    assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    assert!(!h.db().get_tunnel()?.enabled);

    // With the confirmation it turns on and records when.
    let ok = post(&h, &s, "/online/enable", "confirm=yes").await;
    assert_eq!(ok.status, StatusCode::SEE_OTHER, "{}", ok.text());
    assert_eq!(ok.header("location"), Some("/online"));
    let row = h.db().get_tunnel()?;
    assert!(row.enabled);
    assert_eq!(row.consent_unix, Some(T0));
    let page = get(&h, &s, "/online").await.text();
    assert!(page.contains("Getting ready"), "{page}");
    assert!(page.contains("/online/disable"));
    Ok(())
}

#[tokio::test]
async fn the_claim_link_and_code_are_shown_as_text() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    set_state(
        &h,
        true,
        TunnelState::Claiming {
            code: "abc123def0".into(),
            url: "https://playit.gg/claim/abc123def0".into(),
        },
    );
    let page = get(&h, &s, "/online").await.text();
    assert!(
        page.contains("<code>https://playit.gg/claim/abc123def0</code>"),
        "{page}"
    );
    assert!(page.contains("<code>abc123def0</code>"));
    assert!(page.contains("rel=\"noopener noreferrer\""));
    assert!(page.contains("Waiting for you to link playit.gg"));
    assert_no_inline_code(&page);
    // The polled partial is the same card without the layout.
    let partial = get(&h, &s, "/partials/online").await;
    assert_eq!(partial.status, StatusCode::OK);
    let partial = partial.text();
    assert!(partial.contains("hx-get=\"/partials/online\""));
    assert!(!partial.contains("<html"));
    Ok(())
}

#[tokio::test]
async fn the_public_address_shows_on_the_screen_and_on_the_home_card() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    set_state(
        &h,
        true,
        TunnelState::Running {
            address: "mine.craft.playit.gg:12345".into(),
        },
    );
    let page = get(&h, &s, "/online").await.text();
    assert!(
        page.contains("<code>mine.craft.playit.gg:12345</code>"),
        "{page}"
    );
    assert!(page.contains(">On<"));
    // The card needs a server to show anything at all.
    let res = post(
        &h,
        &s,
        "/server/create",
        "name=Test+world&mc_version=26.3&eula=yes",
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    let card = get(&h, &s, "/partials/server-card").await.text();
    assert!(card.contains("mine.craft.playit.gg:12345"), "{card}");
    // Off again: the address disappears from the card.
    set_state(&h, false, TunnelState::Claimed);
    let card = get(&h, &s, "/partials/server-card").await.text();
    assert!(!card.contains("mine.craft.playit.gg"), "{card}");
    Ok(())
}

#[tokio::test]
async fn claimed_tells_the_user_where_the_tunnel_must_point() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    set_state(&h, true, TunnelState::Claimed);
    let page = get(&h, &s, "/online").await.text();
    assert!(page.contains("<code>127.0.0.1:25565</code>"), "{page}");
    Ok(())
}

#[tokio::test]
async fn errors_are_escaped() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    set_state(
        &h,
        true,
        TunnelState::Error {
            reason: "<script>alert(1)</script> <img src=x onerror=alert(2)>".into(),
        },
    );
    let page = get(&h, &s, "/online").await.text();
    assert!(!page.contains("<script>alert"), "{page}");
    assert!(!page.contains("<img src=x"), "{page}");
    assert!(page.contains("&#60;script&#62;alert(1)"), "{page}");
    assert!(page.contains("Problem"));
    assert!(page.contains("It is retried automatically."));
    assert_no_inline_code(&page);
    Ok(())
}

#[tokio::test]
async fn disabling_needs_no_confirmation_but_a_csrf_token() -> TestResult {
    let h = harness();
    let s = h.pair("Pixel").await;
    set_state(&h, true, TunnelState::Claimed);
    let no_csrf = h
        .send(Rb::post("/online/disable").cookie(&s.token).form(""))
        .await;
    assert_eq!(no_csrf.status, StatusCode::FORBIDDEN);
    assert!(h.db().get_tunnel()?.enabled);

    let off = post(&h, &s, "/online/disable", "").await;
    assert_eq!(off.status, StatusCode::SEE_OTHER, "{}", off.text());
    let row = h.db().get_tunnel()?;
    assert!(!row.enabled);
    assert_eq!(row.state, TunnelState::NotConfigured, "no secret file here");
    let page = get(&h, &s, "/online").await.text();
    assert!(page.contains(">Off<"));

    // The unlink variant goes through the same route.
    set_state(&h, true, TunnelState::Claimed);
    let off = post(&h, &s, "/online/disable", "forget=yes").await;
    assert_eq!(off.status, StatusCode::SEE_OTHER);
    assert!(!h.db().get_tunnel()?.enabled);
    Ok(())
}

#[tokio::test]
async fn a_daemon_without_online_play_says_so() -> TestResult {
    let h = harness_without_tunnel();
    let s = h.pair("Pixel").await;
    let page = get(&h, &s, "/online").await.text();
    assert!(page.contains("not available in this daemon"), "{page}");
    assert!(!page.contains("/online/enable"));
    // Enabling is refused with a sentence, not a crash.
    let res = post(&h, &s, "/online/enable", "confirm=yes").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.text());
    Ok(())
}
