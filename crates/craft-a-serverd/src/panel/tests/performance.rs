// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! Tests of the Performance screen: the resource card, the view-distance
//! form, refusal of bad values, CSRF and that nothing inline sneaks in.
//! Starting a container needs Linux-style paths for the fake runtime, so the
//! tests that do run on unix only.

use super::*;

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

fn private_harness() -> (Harness, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    (harness_with(move |c| c.data_dir = path), dir)
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

fn first_id(h: &Harness) -> String {
    h.svc
        .list_servers()
        .expect("list")
        .into_iter()
        .next()
        .expect("a server")
        .id
}

fn assert_no_inline_code(page: &str) {
    let body = page.replace("<script src=\"/static/htmx.min.js\" defer></script>", "");
    for needle in ["<script", " style=", " onclick=", " onload=", "javascript:"] {
        assert!(!body.contains(needle), "found {needle} in {body}");
    }
}

#[tokio::test]
async fn the_screen_is_in_the_navigation_and_needs_a_server() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let res = get(&h, &s, "/performance").await;
    assert_eq!(res.status, StatusCode::OK);
    let page = res.text();
    assert!(
        page.contains("href=\"/performance\" aria-current=\"page\""),
        "{page}"
    );
    assert!(page.contains("There is no server yet"), "{page}");
    assert!(!page.contains("name=\"view_distance\""));
    assert_no_inline_code(&page);
    assert!(
        get(&h, &s, "/")
            .await
            .text()
            .contains("href=\"/performance\"")
    );
    Ok(())
}

#[tokio::test]
async fn the_form_lists_the_allowed_distances_and_marks_the_default_as_current() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let page = get(&h, &s, "/performance").await.text();
    assert!(
        page.contains("action=\"/performance/view-distance\""),
        "{page}"
    );
    assert!(page.contains("name=\"csrf_token\""));
    // Nothing chosen yet: the game is on its own default, which is labelled.
    assert!(
        page.contains("<option value=\"10\" selected>10 (game default)</option>"),
        "{page}"
    );
    assert!(!page.contains("value=\"default\""));
    assert!(page.contains("<option value=\"4\">4</option>"));
    assert!(page.contains("<option value=\"16\">16</option>"));
    assert!(!page.contains("<option value=\"3\">"));
    assert!(!page.contains("<option value=\"17\">"));
    assert!(page.contains("takes effect the next time the server starts"));
    assert_no_inline_code(&page);
    Ok(())
}

#[tokio::test]
async fn saving_a_distance_stores_it_and_shows_it_selected() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let id = first_id(&h);

    let res = post(&h, &s, "/performance/view-distance", "view_distance=8").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    assert_eq!(res.header("location"), Some("/performance"));
    assert_eq!(h.svc.view_distance(&id).expect("read"), Some(8));

    let page = get(&h, &s, "/performance").await.text();
    assert!(page.contains("<option value=\"8\" selected>"), "{page}");
    assert!(!page.contains("<option value=\"10\" selected>"));
    assert!(page.contains("View distance saved"), "{page}");

    // Back to the game's default is an explicit 10, stored like any other.
    let res = post(&h, &s, "/performance/view-distance", "view_distance=10").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert_eq!(h.svc.view_distance(&id).expect("read"), Some(10));
    Ok(())
}

#[tokio::test]
async fn bad_values_are_refused_and_change_nothing() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let id = first_id(&h);
    post(&h, &s, "/performance/view-distance", "view_distance=6").await;

    for bad in ["3", "17", "255", "256", "-1", "abc", "default", "8.5", ""] {
        let res = post(
            &h,
            &s,
            "/performance/view-distance",
            &format!("view_distance={bad}"),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "accepted {bad:?}");
    }
    // A missing field is refused too.
    let res = post(&h, &s, "/performance/view-distance", "").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(h.svc.view_distance(&id).expect("read"), Some(6));
    Ok(())
}

#[tokio::test]
async fn saving_needs_a_server_a_session_and_the_csrf_token() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    // No server yet.
    let res = post(&h, &s, "/performance/view-distance", "view_distance=8").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.text());

    create(&h, &s).await;
    // A session without the CSRF token.
    let res = h
        .send(
            Rb::post("/performance/view-distance")
                .cookie(&s.token)
                .form("view_distance=8"),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.text());
    // No session at all.
    let res = h
        .send(Rb::post("/performance/view-distance").form("view_distance=8"))
        .await;
    assert!(
        matches!(res.status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
        "{}",
        res.status
    );
    assert_eq!(h.svc.view_distance(&first_id(&h)).expect("read"), None);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_running_server_shows_what_it_uses() -> TestResult {
    use crate::runtime::ContainerStats;
    use std::time::Duration;

    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let id = first_id(&h);

    // Stopped: says so.
    let page = get(&h, &s, "/partials/performance").await.text();
    assert!(page.contains("The server is not running"), "{page}");

    h.svc.probe.set_up(true);
    h.svc.start(&id).await.expect("start");
    h.svc.rt.set_stats(
        &id,
        ContainerStats {
            mem_used_bytes: 3 << 30,
            cpu_time: Duration::from_secs(20),
            pids: 61,
        },
    );
    let page = get(&h, &s, "/partials/performance").await.text();
    assert!(page.contains("id=\"perf-usage\""), "{page}");
    assert!(page.contains("hx-get=\"/partials/performance\""));
    assert!(page.contains("3.0 GiB"), "{page}");
    assert!(page.contains("61 (limit"), "{page}");
    assert!(page.contains("Measuring..."), "{page}");
    assert!(page.contains("<progress"), "{page}");
    assert_no_inline_code(&page);
    Ok(())
}
