// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! Tests of the screens: home, create, backups, players, logs, mods and
//! settings. They go through the real router like the security tests do.
//! Tests that start a container need Linux-style paths for the fake runtime,
//! so they run on unix only (WSL on a Windows dev box).

use std::time::Duration;

use super::*;
use crate::runtime::RuntimeCall;

type Td = tempfile::TempDir;

/// A harness whose data directory is private to the test.
fn screens_harness(tweak: impl FnOnce(&mut Config) + 'static) -> (Harness, Td) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    let h = harness_with(move |c| {
        c.data_dir = path;
        tweak(c);
    });
    (h, dir)
}

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

fn location(res: &Resp) -> &str {
    res.header("location").unwrap_or_default()
}

/// Waits until the background action (if any) has finished.
async fn idle(h: &Harness, s: &Sess) {
    for _ in 0..1000 {
        let card = get(h, s, "/partials/server-card").await.text();
        if !card.contains("notice-busy") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the background action never finished");
}

async fn create(h: &Harness, s: &Sess) {
    let res = post(
        h,
        s,
        "/server/create",
        "name=Test+world&mc_version=26.3&eula=yes",
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
}

#[cfg_attr(not(unix), allow(dead_code))]
fn server_id(h: &Harness) -> String {
    h.svc
        .list_servers()
        .expect("list")
        .into_iter()
        .next()
        .expect("a server")
        .id
}

fn rcon_commands(h: &Harness) -> Vec<String> {
    h.svc
        .rt
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            RuntimeCall::Rcon { command, .. } => Some(command),
            _ => None,
        })
        .collect()
}

#[cfg(unix)]
async fn create_and_start(h: &Harness, s: &Sess) {
    create(h, s).await;
    h.svc.probe.set_up(true);
    let res = post(h, s, "/server/start", "").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    idle(h, s).await;
}

// ------------------------------------------------------------- every screen

#[tokio::test]
async fn every_screen_renders_with_its_navigation_entry() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    for (path, active, needle) in [
        (
            "/",
            "href=\"/\" aria-current=\"page\"",
            "You have no server yet.",
        ),
        ("/create", "href=\"/\" aria-current", "Create your server"),
        (
            "/backups",
            "href=\"/backups\" aria-current=\"page\"",
            "Backups",
        ),
        (
            "/players",
            "href=\"/players\" aria-current=\"page\"",
            "Start the server to manage players.",
        ),
        (
            "/logs",
            "href=\"/logs\" aria-current=\"page\"",
            "/logs/tail",
        ),
        ("/mods", "href=\"/mods\" aria-current=\"page\"", "no server"),
        (
            "/settings/devices",
            "href=\"/settings/devices\" aria-current=\"page\"",
            "Panel address",
        ),
    ] {
        let res = get(&h, &s, path).await;
        assert_eq!(res.status, StatusCode::OK, "{path}");
        let html = res.text();
        assert!(html.contains(needle), "{path}: {html}");
        assert!(html.contains(active), "{path}: nav entry\n{html}");
        // The CSP allows no inline script, style or handler.
        assert!(
            !html.contains("<style") && !html.contains(" style="),
            "{path}"
        );
        let lower = html.to_lowercase();
        for handler in [
            " onclick=",
            " onload=",
            " onerror=",
            " onsubmit=",
            "javascript:",
        ] {
            assert!(!lower.contains(handler), "{path}: {handler}");
        }
        for (i, _) in html.match_indices("<script") {
            let tag = html
                .get(i..)
                .and_then(|r| r.split('>').next())
                .unwrap_or_default();
            assert!(
                tag.contains(" src=\"/static/"),
                "{path}: inline script {tag}"
            );
        }
        assert_security_headers(&res, path);
    }
    Ok(())
}

#[tokio::test]
async fn polled_partials_are_marked_for_htmx_with_the_csrf_header_inherited() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    let home = get(&h, &s, "/").await.text();
    assert!(
        home.contains("hx-get=\"/partials/server-card\"")
            && home.contains("hx-trigger=\"every 3s\"")
    );
    let logs = get(&h, &s, "/logs").await.text();
    assert!(logs.contains("hx-trigger=\"load, every 2s\""));
    // htmx reads its CSRF header from <body hx-headers>, which the layout sets.
    assert!(home.contains("X-CSRF-Token"));
    Ok(())
}

#[tokio::test]
async fn every_state_changing_screen_route_needs_the_csrf_token() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    for (path, access, methods) in EXPECTED_ROUTES {
        if *access != Access::Session || !methods.contains(&Method::POST) {
            continue;
        }
        let url = concrete(path);
        // No token at all, then a wrong one in the header and in the form.
        let none = h
            .send(Rb::post(&url).cookie(&s.token).form("name=abc&confirm=yes"))
            .await;
        assert_eq!(none.status, StatusCode::FORBIDDEN, "{path}");
        let wrong_hdr = h
            .send(
                Rb::post(&url)
                    .cookie(&s.token)
                    .csrf("nope")
                    .form("confirm=yes"),
            )
            .await;
        assert_eq!(wrong_hdr.status, StatusCode::FORBIDDEN, "{path}");
        let wrong_form = h
            .send(
                Rb::post(&url)
                    .cookie(&s.token)
                    .form("csrf_token=nope&confirm=yes"),
            )
            .await;
        assert_eq!(wrong_form.status, StatusCode::FORBIDDEN, "{path}");
        // A foreign Origin is refused before anything else.
        let foreign = h
            .send(
                Rb::post(&url)
                    .cookie(&s.token)
                    .origin("http://evil.example")
                    .form(&fields(&s, "confirm=yes")),
            )
            .await;
        assert_eq!(foreign.status, StatusCode::FORBIDDEN, "{path}");
    }
    // Nothing happened behind those refusals.
    assert!(h.svc.list_servers()?.is_empty());
    assert!(h.svc.rt.calls().is_empty());
    Ok(())
}

#[tokio::test]
async fn state_changing_routes_do_not_answer_get() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    for path in [
        "/server/start",
        "/server/stop",
        "/server/restart",
        "/server/create",
        "/backups/create",
        "/players/whitelist/add",
        "/players/whitelist/remove",
        "/players/kick",
    ] {
        let res = get(&h, &s, path).await;
        assert_eq!(res.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
    Ok(())
}

// ------------------------------------------------------------------ create

#[tokio::test]
async fn the_create_form_shows_memory_and_an_unticked_eula() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    let html = get(&h, &s, "/create").await.text();
    // 8 GiB machine in the harness: 5888 MiB of heap.
    assert!(html.contains("5.7 GiB"), "{html}");
    assert!(html.contains("name=\"eula\" type=\"checkbox\" value=\"yes\" required"));
    assert!(
        !html.contains("checked"),
        "the EULA box must start unticked"
    );
    assert!(html.contains("name=\"csrf_token\""));
    Ok(())
}

#[tokio::test]
async fn a_server_cannot_be_created_without_ticking_the_eula() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    for rest in [
        "name=A&mc_version=26.3",
        "name=A&mc_version=26.3&eula=no",
        "name=A&mc_version=26.3&eula=on",
        "name=A&mc_version=26.3&eula=true",
        "name=A&mc_version=26.3&eula=",
    ] {
        let res = post(&h, &s, "/server/create", rest).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{rest}");
        assert!(
            res.text().contains("You must accept the Minecraft EULA"),
            "{rest}"
        );
        // The form comes back with what was typed.
        assert!(res.text().contains("value=\"A\""));
    }
    assert!(h.svc.list_servers()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn invalid_names_and_versions_come_back_to_the_form() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    let res = post(
        &h,
        &s,
        "/server/create",
        "name=ok&mc_version=..%2Fetc&eula=yes",
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert!(
        res.text().contains("Invalid Minecraft version"),
        "{}",
        res.text()
    );
    let res = post(&h, &s, "/server/create", "name=&mc_version=26.3&eula=yes").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    // Typed text is escaped when echoed back.
    let res = post(
        &h,
        &s,
        "/server/create",
        "name=%3Cscript%3E&mc_version=%22%3E&eula=yes",
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let html = res.text();
    assert!(
        !html.contains("<script>") && html.contains("&#60;script&#62;"),
        "{html}"
    );
    assert!(h.svc.list_servers()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn creating_a_server_works_once_and_the_home_card_shows_it() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let rows = h.svc.list_servers()?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows.first().map(|r| r.name.as_str()), Some("Test world"));
    // Only one server in the UI: the form redirects home and a second create is refused.
    let again = get(&h, &s, "/create").await;
    assert_eq!(
        (again.status, location(&again)),
        (StatusCode::SEE_OTHER, "/")
    );
    let second = post(&h, &s, "/server/create", "name=B&mc_version=26.3&eula=yes").await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(h.svc.list_servers()?.len(), 1);

    let home = get(&h, &s, "/").await.text();
    assert!(
        home.contains("Test world") && home.contains("Stopped"),
        "{home}"
    );
    assert!(home.contains("Server created."), "{home}");
    assert!(home.contains("action=\"/server/start\""));
    // Both ways in: the .local name and the number address.
    assert!(
        home.contains("craft-a-server.local") && home.contains("192.168.1.50"),
        "{home}"
    );
    // No RCON password anywhere.
    let secret = h
        .svc
        .list_servers()?
        .first()
        .map(|r| r.rcon_password.clone())
        .unwrap_or_default();
    assert!(secret.len() >= 16 && !home.contains(&secret));
    Ok(())
}

#[tokio::test]
async fn user_text_is_escaped_on_the_home_card() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    let res = post(
        &h,
        &s,
        "/server/create",
        "name=%3Cimg+src%3Dx+onerror%3Dalert(1)%3E&mc_version=26.3&eula=yes",
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let home = get(&h, &s, "/").await.text();
    assert!(!home.contains("<img"), "{home}");
    assert!(home.contains("&#60;img"), "{home}");
    Ok(())
}

// ------------------------------------------------- lifecycle (unix: fake runtime)

#[cfg(unix)]
#[tokio::test]
async fn start_stop_restart_go_through_the_lifecycle_service() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    let id = server_id(&h);
    let card = get(&h, &s, "/partials/server-card").await.text();
    assert!(
        card.contains("Running") && card.contains("0 / 20"),
        "{card}"
    );
    assert!(card.contains("The server is running."), "{card}");
    assert!(
        card.contains("action=\"/server/stop\"") && card.contains("action=\"/server/restart\"")
    );
    assert!(!card.contains("action=\"/server/start\""));
    assert!(
        h.svc
            .rt
            .calls()
            .contains(&RuntimeCall::CreateAndStart { id: id.clone() })
    );

    let res = post(&h, &s, "/server/restart", "").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    idle(&h, &s).await;
    let creates = h
        .svc
        .rt
        .calls()
        .iter()
        .filter(|c| matches!(c, RuntimeCall::CreateAndStart { .. }))
        .count();
    assert_eq!(creates, 2);

    let res = post(&h, &s, "/server/stop", "").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    idle(&h, &s).await;
    let card = get(&h, &s, "/partials/server-card").await.text();
    assert!(
        card.contains("Stopped") && card.contains("action=\"/server/start\""),
        "{card}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_second_action_while_one_runs_is_refused() -> TestResult {
    let (h, _dir) = screens_harness(|c| c.start_timeout_secs = 60);
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    // The server does not answer yet, so the start keeps waiting.
    let first = post(&h, &s, "/server/start", "").await;
    assert_eq!(first.status, StatusCode::SEE_OTHER);
    let card = get(&h, &s, "/partials/server-card").await.text();
    assert!(card.contains("Starting the server."), "{card}");
    assert!(
        !card.contains("action=\"/server/stop\"") && !card.contains("action=\"/server/start\"")
    );
    let second = post(&h, &s, "/server/start", "").await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert!(second.text().contains("Another action is still running"));
    let backup = post(&h, &s, "/backups/create", "").await;
    assert_eq!(backup.status, StatusCode::CONFLICT);
    h.svc.probe.set_up(true);
    idle(&h, &s).await;
    assert!(
        get(&h, &s, "/partials/server-card")
            .await
            .text()
            .contains("Running")
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_start_shows_why_and_reset_needs_the_confirm_step() -> TestResult {
    let (h, _dir) = screens_harness(|c| c.start_timeout_secs = 1);
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    // The probe never answers, so the start times out and the server fails.
    post(&h, &s, "/server/start", "").await;
    idle(&h, &s).await;
    let card = get(&h, &s, "/partials/server-card").await.text();
    assert!(
        card.contains("Failed") && card.contains("What went wrong"),
        "{card}"
    );
    assert!(card.contains("The server did not start."), "{card}");
    assert!(card.contains("href=\"/server/reset\""));
    // Confirm page first; a bare POST does nothing.
    let page = get(&h, &s, "/server/reset").await.text();
    assert!(
        page.contains("Reset the server?") && page.contains("name=\"confirm\" value=\"yes\""),
        "{page}"
    );
    for rest in ["", "confirm=no", "confirm=1"] {
        let res = post(&h, &s, "/server/reset", rest).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{rest}");
    }
    assert!(
        get(&h, &s, "/partials/server-card")
            .await
            .text()
            .contains("Failed")
    );
    let res = post(&h, &s, "/server/reset", "confirm=yes").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    idle(&h, &s).await;
    assert!(
        get(&h, &s, "/partials/server-card")
            .await
            .text()
            .contains("Stopped")
    );
    Ok(())
}

#[tokio::test]
async fn deleting_needs_the_confirm_step() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let page = get(&h, &s, "/server/delete").await.text();
    assert!(
        page.contains("Delete the server?") && page.contains("cannot be undone"),
        "{page}"
    );
    assert!(page.contains("action=\"/server/delete\""));
    for rest in ["", "confirm=no", "name=Test"] {
        let res = post(&h, &s, "/server/delete", rest).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{rest}");
    }
    assert_eq!(h.svc.list_servers()?.len(), 1);
    let res = post(&h, &s, "/server/delete", "confirm=yes").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    idle(&h, &s).await;
    assert!(h.svc.list_servers()?.is_empty());
    let home = get(&h, &s, "/").await.text();
    assert!(home.contains("You have no server yet.") && home.contains("The server was deleted."));
    Ok(())
}

#[tokio::test]
async fn actions_without_a_server_say_so() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    for path in [
        "/server/start",
        "/server/stop",
        "/server/restart",
        "/backups/create",
    ] {
        let res = post(&h, &s, path, "").await;
        assert_eq!(res.status, StatusCode::CONFLICT, "{path}");
        assert!(res.text().contains("no server yet"), "{path}");
    }
    let res = post(&h, &s, "/players/whitelist/add", "name=Steve").await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert!(h.svc.rt.calls().is_empty());
    Ok(())
}

// ----------------------------------------------------------------- backups

#[cfg(unix)]
#[tokio::test]
async fn backups_can_be_created_and_restored_only_after_confirming() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    let empty = get(&h, &s, "/backups").await.text();
    assert!(
        empty.contains("No backups yet.") && empty.contains("action=\"/backups/create\""),
        "{empty}"
    );

    let res = post(&h, &s, "/backups/create", "").await;
    assert_eq!(
        (res.status, location(&res)),
        (StatusCode::SEE_OTHER, "/backups")
    );
    idle(&h, &s).await;
    let rows = h.svc.list_backups(&server_id(&h))?;
    assert_eq!(rows.len(), 1);
    let backup = rows.first().expect("a backup").clone();
    let list = get(&h, &s, "/partials/backups").await.text();
    assert!(
        list.contains("Manual") && list.contains("Backup created."),
        "{list}"
    );
    assert!(list.contains(&format!("/backups/{}/restore", backup.id)));

    // Confirm page first.
    let confirm = get(&h, &s, &format!("/backups/{}/restore", backup.id))
        .await
        .text();
    assert!(
        confirm.contains("Restore this backup?") && confirm.contains("safety backup"),
        "{confirm}"
    );
    // A POST without the confirm field changes nothing.
    let path = format!("/backups/{}/restore", backup.id);
    for rest in ["", "confirm=no"] {
        let res = post(&h, &s, &path, rest).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{rest}");
    }
    assert_eq!(
        h.svc.list_backups(&server_id(&h))?.len(),
        1,
        "no safety backup yet"
    );

    let res = post(&h, &s, &path, "confirm=yes").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    idle(&h, &s).await;
    // The lifecycle service took its pre-restore safety backup.
    let kinds: Vec<String> = h
        .svc
        .list_backups(&server_id(&h))?
        .into_iter()
        .map(|b| b.kind)
        .collect();
    assert!(kinds.contains(&"pre_restore".to_owned()), "{kinds:?}");
    assert!(
        get(&h, &s, "/partials/backups")
            .await
            .text()
            .contains("Backup restored.")
    );
    Ok(())
}

#[tokio::test]
async fn unknown_or_malformed_backup_ids_are_plain_404s() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    for id in ["nope", "s0000-1-manual", "..%2F..%2Fetc", "a%20b", "%00"] {
        let path = format!("/backups/{id}/restore");
        assert_eq!(
            get(&h, &s, &path).await.status,
            StatusCode::NOT_FOUND,
            "GET {id}"
        );
        let res = post(&h, &s, &path, "confirm=yes").await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "POST {id}");
    }
    Ok(())
}

// ----------------------------------------------------------------- players

#[cfg(unix)]
#[tokio::test]
async fn only_valid_names_reach_the_console_and_only_whitelisted_commands() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    let before = rcon_commands(&h).len();

    for bad in [
        "a",
        "ab",
        "seventeen_chars_xx",
        "bad+name",
        "x%3Bop+me",
        "Steve%0Aop+Steve",
        "Steve%0D%0Aop+Steve",
        "St%C3%A9ve",
        "%E2%80%AEevil",
        "name%00",
        "a-b-c",
        "..%2F..",
        "",
    ] {
        for path in [
            "/players/whitelist/add",
            "/players/whitelist/remove",
            "/players/kick",
        ] {
            let res = post(&h, &s, path, &format!("name={bad}")).await;
            assert_eq!(res.status, StatusCode::SEE_OTHER, "{path} {bad}");
            assert_eq!(location(&res), "/players");
        }
    }
    assert_eq!(
        rcon_commands(&h).len(),
        before,
        "no bad name may reach RCON"
    );
    let page = get(&h, &s, "/players").await.text();
    assert!(
        page.contains("Player names are 3 to 16 letters, digits or underscores"),
        "{page}"
    );

    // Valid names produce exactly the closed command set.
    let before = rcon_commands(&h).len();
    post(&h, &s, "/players/whitelist/add", "name=Steve_1").await;
    post(&h, &s, "/players/kick", "name=Alex").await;
    post(&h, &s, "/players/whitelist/remove", "name=Steve_1").await;
    let sent: Vec<String> = rcon_commands(&h).split_off(before);
    assert_eq!(
        sent,
        vec![
            "whitelist add Steve_1",
            "kick Alex",
            "whitelist remove Steve_1"
        ]
    );
    // Each name is exactly the allowed shape.
    assert!(
        get(&h, &s, "/players")
            .await
            .text()
            .contains("Removed from the list: Steve_1")
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn the_players_page_lists_the_whitelist_escaped() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    h.svc.rt.set_rcon_reply(
        "whitelist list",
        "There are 3 whitelisted player(s): Zed, Alice, <script>alert(1)</script>",
    );
    let page = get(&h, &s, "/players").await.text();
    assert!(page.contains("Alice") && page.contains("Zed"), "{page}");
    assert!(
        !page.contains("<script>alert") && !page.contains("alert(1)"),
        "{page}"
    );
    assert!(
        page.contains("action=\"/players/kick\"")
            && page.contains("action=\"/players/whitelist/remove\"")
    );
    assert!(page.contains("pattern=\"[A-Za-z0-9_]{3,16}\""));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn the_server_reply_is_shown_escaped_and_short() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    let long = format!("<b>That player does not exist</b>{}", "x".repeat(500));
    h.svc.rt.set_rcon_reply("whitelist add Ghost", &long);
    post(&h, &s, "/players/whitelist/add", "name=Ghost").await;
    let page = get(&h, &s, "/players").await.text();
    assert!(
        page.contains("&#60;b&#62;That player does not exist"),
        "{page}"
    );
    assert!(!page.contains(&"x".repeat(200)));
    Ok(())
}

#[tokio::test]
async fn player_commands_need_a_running_server() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let res = post(&h, &s, "/players/whitelist/add", "name=Steve").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    let page = get(&h, &s, "/players").await.text();
    assert!(
        page.contains("The server must be running for that"),
        "{page}"
    );
    assert!(rcon_commands(&h).is_empty());
    Ok(())
}

// -------------------------------------------------------------------- logs

#[cfg(unix)]
#[tokio::test]
async fn log_lines_are_html_escaped_and_cleaned() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    let id = server_id(&h);
    h.svc
        .rt
        .push_log(&id, "[12:00:00] <Eve> <script>alert('x')</script>");
    h.svc.rt.push_log(
        &id,
        "[12:00:01] <Eve> <img src=x onerror=alert(1)> &amp; \u{1b}[31mred\u{1b}[0m",
    );
    h.svc
        .rt
        .push_log(&id, "[12:00:02] ok\u{0}\u{7}\u{202e}fine");
    let res = get(&h, &s, "/logs/tail").await;
    assert_eq!(res.status, StatusCode::OK);
    let html = res.text();
    assert!(
        !html.contains("<script>") && !html.contains("<img"),
        "{html}"
    );
    assert!(
        html.contains("&#60;script&#62;alert(&#39;x&#39;)&#60;/script&#62;"),
        "{html}"
    );
    assert!(
        html.contains("&#60;img src=x onerror=alert(1)&#62; &#38;amp; red"),
        "{html}"
    );
    assert!(html.contains("okfine"), "{html}");
    assert!(!html.contains('\u{1b}') && !html.contains('\u{202e}'));
    assert_security_headers(&res, "logs tail");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn log_output_is_capped() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    let id = server_id(&h);
    for i in 0..350 {
        h.svc.rt.push_log(&id, &format!("line {i}"));
    }
    h.svc.rt.push_log(&id, &"w".repeat(5000));
    let html = get(&h, &s, "/logs/tail").await.text();
    assert!(html.contains("line 349") && !html.contains("line 100\n"));
    assert!(!html.contains(&"w".repeat(401)));
    let lines = html
        .lines()
        .filter(|l| l.contains("line ") || l.starts_with('w'))
        .count();
    assert!(lines <= 200, "{lines}");
    Ok(())
}

#[tokio::test]
async fn logs_without_a_server_or_a_container_are_empty_not_errors() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    let res = get(&h, &s, "/logs/tail").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.text().contains("No log lines yet."));
    create(&h, &s).await;
    let res = get(&h, &s, "/logs/tail").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.text().contains("No log lines yet."));
    Ok(())
}

// ---------------------------------------------------------------- settings

#[tokio::test]
async fn settings_show_the_ip_url_an_inline_qr_and_the_devices() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    let other = h.pair("Tablet").await;
    let res = get(&h, &s, "/settings/devices").await;
    let html = res.text();
    assert!(html.contains("http://craft-a-server.local:8080/"), "{html}");
    assert!(html.contains("http://192.168.1.50:8080/"), "{html}");
    assert!(
        html.contains("<svg ") && html.contains("class=\"qr\""),
        "{html}"
    );
    assert!(html.contains("Pixel") && html.contains("Tablet") && html.contains("this phone"));
    // The QR carries the address only; no cookie, token or pairing code.
    let svg = html
        .split("<svg ")
        .nth(1)
        .and_then(|r| r.split("</svg>").next())
        .unwrap_or_default();
    for secret in [&s.token, &other.token, &other.csrf] {
        assert!(!html.contains(secret.as_str()));
    }
    assert!(!svg.contains(s.csrf.as_str()));
    // The CSP allows inline <svg> elements but nothing in it may be active.
    assert!(!svg.contains("style=") && !svg.contains("<script") && !svg.contains("href"));

    // Revoking the other phone works from this page.
    let revoke = format!("/api/sessions/{}/revoke", other.id);
    let res = post(&h, &s, &revoke, "").await;
    assert_eq!(
        (res.status, location(&res)),
        (StatusCode::SEE_OTHER, "/settings/devices")
    );
    assert_eq!(get(&h, &other, "/").await.status, StatusCode::SEE_OTHER);
    Ok(())
}

#[tokio::test]
async fn settings_explain_when_no_ip_address_is_known() -> TestResult {
    let (h, _dir) = {
        let dir = tempfile::tempdir()?;
        let path = dir.path().to_path_buf();
        (harness_full(move |c| c.data_dir = path, None), dir)
    };
    let s = h.pair("Pixel").await;
    let html = get(&h, &s, "/settings/devices").await.text();
    assert!(html.contains("could not be determined"), "{html}");
    assert!(!html.contains("<svg "));
    Ok(())
}

// -------------------------------------------------------------------- mods

const ALPHA: &str = "AAAAAAAA";
const BETA: &str = "BBBBBBBB";
const EVIL: &str = "CCCCCCCC";

/// Alpha (needs Beta), Beta, and a project whose texts try to inject markup.
fn seed_modrinth(h: &Harness) {
    use crate::mods::testutil::{Synth, serve_synth};
    serve_synth(&h.net, "Beta Lib", "required", &[Synth::new(BETA, "b1")]);
    serve_synth(
        &h.net,
        "Alpha Mod",
        "optional",
        &[Synth::new(ALPHA, "a1").dep(BETA, "required")],
    );
    serve_synth(
        &h.net,
        "<script>alert(1)</script>",
        "required",
        &[Synth::new(EVIL, "c1")],
    );
    let p = |id: &str, title: &str| serde_json::json!({"id": id, "title": title, "project_type": "mod", "server_side": "required"});
    h.net.api(
        "/projects",
        &serde_json::json!([
            p(ALPHA, "Alpha Mod"),
            p(BETA, "Beta Lib"),
            p(EVIL, "<script>alert(1)</script>")
        ]),
    );
    h.net.api(
        "/search",
        &serde_json::json!({"hits": [
            {"project_id": ALPHA, "slug": "alpha", "title": "<img src=x onerror=alert(1)>",
             "description": "<b>bold</b> & \"quoted\"", "author": "<i>me</i>", "downloads": 1_234_567,
             "server_side": "optional", "project_type": "mod",
             "icon_url": "https://cdn.modrinth.com/data/AAAAAAAA/icon.png"},
            {"project_id": BETA, "slug": "beta", "title": "Beta Lib", "description": "lib",
             "author": "x", "downloads": 5, "server_side": "required", "project_type": "mod"},
        ]}),
    );
}

#[tokio::test]
async fn the_mods_screen_without_a_server_says_so_and_has_no_form() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    let html = get(&h, &s, "/mods").await.text();
    assert!(html.contains("no server"), "{html}");
    assert!(!html.contains("<form"), "{html}");
    assert!(!html.contains("Coming soon"));
    Ok(())
}

#[tokio::test]
async fn search_results_are_escaped_badged_and_have_no_images() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    seed_modrinth(&h);
    let res = get(&h, &s, "/mods?q=alpha").await;
    assert_eq!(res.status, StatusCode::OK);
    let html = res.text();
    assert!(html.contains("&#60;img"), "{html}");
    assert!(!html.contains("<img"), "{html}");
    assert!(!html.contains("<b>bold</b>") && !html.contains("<i>me</i>"));
    assert!(html.contains("Server: optional") && html.contains("Server: required"));
    assert!(html.contains("1.2M downloads"), "{html}");
    assert!(html.contains(&format!("/mods/install/{ALPHA}")));
    assert!(
        !html.contains("cdn.modrinth.com"),
        "no remote icon is loaded"
    );
    Ok(())
}

#[tokio::test]
async fn a_failing_search_shows_a_calm_message() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    h.net.route(
        "https://api.modrinth.com/v2/search",
        vec![crate::mods::testutil::Reply::Status(400)],
    );
    let res = get(&h, &s, "/mods?q=alpha").await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.text().contains("role=\"alert\""));
    Ok(())
}

#[tokio::test]
async fn the_install_page_lists_dependencies_and_offers_the_confirm_button() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    seed_modrinth(&h);
    let html = get(&h, &s, &format!("/mods/install/{ALPHA}")).await.text();
    assert!(html.contains("Alpha Mod"), "{html}");
    assert!(
        html.contains("Beta Lib"),
        "the dependency is listed\n{html}"
    );
    assert!(html.contains("These required mods will be installed too"));
    assert!(html.contains("name=\"confirm\" value=\"yes\""));
    assert!(html.contains("name=\"csrf_token\""));
    // Hostile titles are escaped here too.
    let evil = get(&h, &s, &format!("/mods/install/{EVIL}")).await.text();
    assert!(evil.contains("&#60;script&#62;"), "{evil}");
    assert!(!evil.contains("<script>alert"), "{evil}");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn the_install_page_of_a_running_server_says_it_will_restart() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create_and_start(&h, &s).await;
    seed_modrinth(&h);
    let html = get(&h, &s, &format!("/mods/install/{ALPHA}")).await.text();
    assert!(html.contains("This will restart the server"), "{html}");
    Ok(())
}

#[tokio::test]
async fn bad_project_ids_never_reach_modrinth() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    for path in [
        "/mods/install/..%2F..%2Fetc",
        "/mods/install/has%20space",
        "/mods/remove/%3Cscript%3E",
        "/mods/remove/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let res = get(&h, &s, path).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{path}: {}", res.status);
    }
    let res = post(&h, &s, "/mods/install/not$valid", "confirm=yes").await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert!(h.net.requests().is_empty(), "nothing reached Modrinth");
    Ok(())
}

#[tokio::test]
async fn installing_and_removing_need_the_confirm_field() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    seed_modrinth(&h);
    for path in [
        format!("/mods/install/{ALPHA}"),
        format!("/mods/remove/{ALPHA}"),
    ] {
        let res = post(&h, &s, &path, "").await;
        assert!(res.status.is_client_error(), "{path}: {}", res.status);
        let res = post(&h, &s, &path, "confirm=no").await;
        assert!(res.status.is_client_error(), "{path}: {}", res.status);
    }
    assert!(h.svc.db().list_mods(&server_id(&h))?.is_empty());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_confirmed_install_runs_in_the_background_and_shows_in_the_list() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    seed_modrinth(&h);

    let res = post(&h, &s, &format!("/mods/install/{ALPHA}"), "confirm=yes").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    assert_eq!(location(&res), "/mods");
    idle(&h, &s).await;

    let rows = h.svc.db().list_mods(&server_id(&h))?;
    assert_eq!(rows.len(), 2);
    let page = get(&h, &s, "/mods").await.text();
    assert!(
        page.contains("Alpha Mod") && page.contains("Beta Lib"),
        "{page}"
    );
    assert!(page.contains("Needed by"), "{page}");
    assert!(page.contains(&format!("/mods/remove/{BETA}")));
    let list = get(&h, &s, "/partials/mods").await.text();
    assert!(list.contains("The mod was installed."), "{list}");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn removal_shows_dependents_and_runs_after_confirmation() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    seed_modrinth(&h);
    post(&h, &s, &format!("/mods/install/{ALPHA}"), "confirm=yes").await;
    idle(&h, &s).await;

    let html = get(&h, &s, &format!("/mods/remove/{BETA}")).await.text();
    assert!(html.contains("Other mods need this one"), "{html}");
    assert!(html.contains("Alpha Mod"), "{html}");
    // Snapshots are named by the second: let the clock move between changes.
    h.clock.fetch_add(60, std::sync::atomic::Ordering::SeqCst);
    // Refused (without force the service says no); the records stay.
    let res = post(&h, &s, &format!("/mods/remove/{BETA}"), "confirm=yes").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    idle(&h, &s).await;
    assert_eq!(h.svc.db().list_mods(&server_id(&h))?.len(), 2);

    h.clock.fetch_add(60, std::sync::atomic::Ordering::SeqCst);
    // Alpha with its automatic dependency.
    let page = get(&h, &s, &format!("/mods/remove/{ALPHA}")).await.text();
    assert!(page.contains("Beta Lib"), "{page}");
    let res = post(
        &h,
        &s,
        &format!("/mods/remove/{ALPHA}"),
        "confirm=yes&orphans=yes",
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    idle(&h, &s).await;
    let left = get(&h, &s, "/partials/mods").await.text();
    assert!(h.svc.db().list_mods(&server_id(&h))?.is_empty(), "{left}");
    Ok(())
}

#[tokio::test]
async fn the_update_page_says_all_is_current_when_nothing_is_installed() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let html = get(&h, &s, "/mods/updates").await.text();
    assert!(html.contains("up to date"), "{html}");
    Ok(())
}

#[tokio::test]
async fn new_screens_require_a_session() -> TestResult {
    let (h, _dir) = screens_harness(|_| {});
    for path in [
        "/",
        "/create",
        "/backups",
        "/partials/backups",
        "/partials/server-card",
        "/players",
        "/logs",
        "/logs/tail",
        "/mods",
        "/partials/mods",
        "/mods/updates",
        "/mods/install/0123456789abcdef",
        "/mods/remove/0123456789abcdef",
        "/settings/devices",
        "/server/reset",
        "/server/delete",
        "/backups/0123456789abcdef/restore",
    ] {
        let res = h.send(Rb::get(path)).await;
        assert_eq!(res.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(location(&res), "/pair");
        // A polling htmx request is told to go to the pair page.
        let hx = h.send(Rb::get(path).hdr("hx-request", "true")).await;
        assert_eq!(hx.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(hx.header("hx-redirect"), Some("/pair"));
    }
    Ok(())
}
