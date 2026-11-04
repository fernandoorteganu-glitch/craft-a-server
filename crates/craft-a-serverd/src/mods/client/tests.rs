// Test code: a wrong shape should fail loudly.
#![allow(clippy::panic, clippy::expect_used)]

//! Client tests over the fake transport: no network.

use std::sync::{Arc, Mutex, PoisonError};

use super::*;
use crate::mods::testutil::{FakeTransport, Reply, Synth, fixture};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn planned(s: &Synth) -> PlannedFile {
    PlannedFile {
        file_name: s.file_name.clone(),
        url: s.url(),
        sha512: s.sha512(),
        size: s.jar.len() as u64,
    }
}

fn query(text: &str, unknown: bool) -> SearchQuery<'_> {
    SearchQuery {
        text,
        mc_version: "26.3",
        include_unknown: unknown,
        offset: 0,
        limit: 20,
        category: None,
        sort: SearchSort::Auto,
    }
}

fn decoded_facets(url: &str) -> String {
    let q = url.split_once('?').map_or("", |(_, q)| q);
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(q).unwrap_or_default();
    pairs
        .into_iter()
        .find(|(k, _)| k == "facets")
        .map(|(_, v)| v)
        .unwrap_or_default()
}

#[test]
fn search_asks_for_server_side_fabric_mods_of_this_game_version() -> TestResult {
    let t = FakeTransport::new();
    t.api("/search", &fixture("search_fabric_26.3_server_ok.json"));
    let hits = t.client().search(query("fabric api", false))?;
    assert_eq!(hits.len(), 5);
    let urls = t.request_urls();
    let facets = decoded_facets(urls.first().expect("one request"));
    for needle in [
        "project_type:mod",
        "categories:fabric",
        "versions:26.3",
        "server_side:required",
        "server_side:optional",
    ] {
        assert!(facets.contains(needle), "{needle} missing in {facets}");
    }
    assert!(!facets.contains("unknown"));
    Ok(())
}

#[test]
fn including_unknown_only_excludes_unsupported() -> TestResult {
    let t = FakeTransport::new();
    t.api("/search", &fixture("search_fabric_26.3_server_ok.json"));
    t.client().search(query("x", true))?;
    let urls = t.request_urls();
    let facets = decoded_facets(urls.first().expect("one request"));
    assert!(facets.contains("server_side!=unsupported"));
    assert!(!facets.contains("server_side:required"));
    Ok(())
}

#[test]
fn client_only_and_foreign_hits_are_dropped_from_the_answer() -> TestResult {
    let t = FakeTransport::new();
    t.api(
        "/search",
        &serde_json::json!({"hits": [
            {"project_id": "AAAAAAAA", "title": "ok", "server_side": "optional", "project_type": "mod"},
            {"project_id": "BBBBBBBB", "title": "client", "server_side": "unsupported", "project_type": "mod"},
            {"project_id": "CCCCCCCC", "title": "pack", "server_side": "required", "project_type": "modpack"},
            {"project_id": "../../x", "title": "evil", "server_side": "required", "project_type": "mod"},
        ]}),
    );
    let hits = t.client().search(query("", false))?;
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits.first().map(|h| h.project_id.as_str()),
        Some("AAAAAAAA")
    );
    Ok(())
}

#[test]
fn hostile_search_input_is_cleaned_and_bad_versions_are_refused() {
    let t = FakeTransport::new();
    t.api("/search", &serde_json::json!({"hits": []}));
    let c = t.client();
    assert!(c.search(query("a\r\nb&c=d", false)).is_ok());
    let url = t.request_urls().remove(0);
    assert!(!url.contains('\n') && !url.contains("&c=d"));
    let bad = SearchQuery {
        mc_version: "1.21&x=1",
        ..query("a", false)
    };
    assert!(matches!(c.search(bad), Err(ModError::InvalidInput(_))));
    assert!(matches!(
        c.project("../etc/passwd"),
        Err(ModError::InvalidInput(_))
    ));
}

#[test]
fn rate_limit_answers_are_retried_after_the_announced_wait() -> TestResult {
    let t = FakeTransport::new();
    t.route(
        "https://api.modrinth.com/v2/project/AAAAAAAA",
        vec![
            Reply::Status(429),
            Reply::Status(503),
            Reply::Json(br#"{"id":"AAAAAAAA","title":"T"}"#.to_vec()),
        ],
    );
    let waits = Arc::new(Mutex::new(Vec::new()));
    let w = Arc::clone(&waits);
    let client = ModrinthClient::new(Arc::new(t.clone()))
        .with_sleeper(move |d| w.lock().unwrap_or_else(PoisonError::into_inner).push(d));
    let p = client.project("AAAAAAAA")?.expect("found");
    assert_eq!(p.id, "AAAAAAAA");
    assert_eq!(t.requests().len(), 3);
    let waits = waits.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(waits.iter().any(|d| *d >= Duration::from_secs(1)));
    Ok(())
}

#[test]
fn retries_stop_and_report_the_status() {
    let t = FakeTransport::new();
    t.route(
        "https://api.modrinth.com/v2/project/",
        vec![Reply::Status(500)],
    );
    let err = t.client().project("AAAAAAAA").err();
    assert!(matches!(err, Some(ModError::Http(HttpError::Status(500)))));
    assert_eq!(t.requests().len(), 4); // first try plus three retries
}

#[test]
fn dropped_connections_are_retried() -> TestResult {
    let t = FakeTransport::new();
    t.route(
        "https://api.modrinth.com/v2/project/",
        vec![
            Reply::NetworkError,
            Reply::Json(br#"{"id":"AAAAAAAA","title":"T"}"#.to_vec()),
        ],
    );
    assert!(t.client().project("AAAAAAAA")?.is_some());
    assert_eq!(t.requests().len(), 2);
    Ok(())
}

#[test]
fn not_found_is_none_and_bad_json_is_an_error() -> TestResult {
    let t = FakeTransport::new();
    assert!(t.client().project("AAAAAAAA")?.is_none());
    t.route(
        "https://api.modrinth.com/v2/version/",
        vec![Reply::Json(b"<html>".to_vec())],
    );
    assert!(matches!(
        t.client().version("AAAAAAAA"),
        Err(ModError::BadResponse(_))
    ));
    Ok(())
}

#[test]
fn requests_are_paced_by_the_limiter() -> TestResult {
    let t = FakeTransport::new();
    let waits = Arc::new(Mutex::new(Vec::new()));
    let w = Arc::clone(&waits);
    let client = ModrinthClient::new(Arc::new(t))
        .with_sleeper(move |d| w.lock().unwrap_or_else(PoisonError::into_inner).push(d));
    for _ in 0..12 {
        client.project("AAAAAAAA")?;
    }
    // Past the burst of 5, calls must wait.
    assert!(
        !waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
    Ok(())
}

#[test]
fn a_verified_download_lands_under_its_name_and_leaves_no_part_file() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    t.cdn(&s.url(), &s.jar);
    let dir = tempfile::tempdir()?;
    let path = t.client().download(&planned(&s), dir.path())?;
    assert_eq!(std::fs::read(&path)?, s.jar);
    let names: Vec<_> = std::fs::read_dir(dir.path())?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, std::slice::from_ref(&s.file_name));
    Ok(())
}

fn nothing_left(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none())
}

#[test]
fn a_hash_mismatch_is_refused_and_cleaned_up() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    // Same size is not required; the content differs from what was announced.
    let mut tampered = s.jar.clone();
    if let Some(b) = tampered.get_mut(10) {
        *b ^= 0xff;
    }
    t.cdn(&s.url(), &tampered);
    let dir = tempfile::tempdir()?;
    let err = t.client().download(&planned(&s), dir.path()).err();
    assert!(
        matches!(err, Some(ModError::HashMismatch { .. })),
        "{err:?}"
    );
    assert!(nothing_left(dir.path()));
    Ok(())
}

#[test]
fn a_foreign_host_is_refused_before_any_request() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    let dir = tempfile::tempdir()?;
    for url in [
        "https://evil.example/mod.jar",
        "http://cdn.modrinth.com/data/x/mod.jar",
        "https://cdn.modrinth.com.evil.example/mod.jar",
        "https://cdn.modrinth.com@evil.example/mod.jar",
        "https://cdn.modrinth.com:8443/mod.jar",
    ] {
        let mut f = planned(&s);
        f.url = url.to_owned();
        let err = t.client().download(&f, dir.path()).err();
        assert!(
            matches!(err, Some(ModError::ForbiddenHost(_))),
            "{url}: {err:?}"
        );
    }
    assert!(t.requests().is_empty());
    assert!(nothing_left(dir.path()));
    Ok(())
}

#[test]
fn a_redirect_is_an_error_not_followed() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    t.route(&s.url(), vec![Reply::Redirect(302)]);
    let dir = tempfile::tempdir()?;
    let err = t.client().download(&planned(&s), dir.path()).err();
    assert!(
        matches!(err, Some(ModError::Http(HttpError::Redirect(302)))),
        "{err:?}"
    );
    assert_eq!(t.requests().len(), 1);
    assert!(nothing_left(dir.path()));
    Ok(())
}

#[test]
fn a_body_larger_than_announced_is_cut_off() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    // The fake does not enforce the cap; the writer must.
    t.route(&s.url(), vec![Reply::Flood(s.jar.len() * 50)]);
    let dir = tempfile::tempdir()?;
    let err = t.client().download(&planned(&s), dir.path()).err();
    assert!(matches!(err, Some(ModError::TooLarge { .. })), "{err:?}");
    assert!(nothing_left(dir.path()));
    Ok(())
}

#[test]
fn announced_sizes_above_the_limit_are_refused_up_front() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    let dir = tempfile::tempdir()?;
    let mut f = planned(&s);
    f.size = MAX_MOD_BYTES + 1;
    assert!(matches!(
        t.client().download(&f, dir.path()).err(),
        Some(ModError::TooLarge { .. })
    ));
    assert!(t.requests().is_empty());
    Ok(())
}

#[test]
fn a_short_body_is_a_size_mismatch() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    t.cdn(&s.url(), s.jar.get(..s.jar.len() - 5).unwrap_or_default());
    let dir = tempfile::tempdir()?;
    let err = t.client().download(&planned(&s), dir.path()).err();
    assert!(
        matches!(err, Some(ModError::SizeMismatch { .. })),
        "{err:?}"
    );
    assert!(nothing_left(dir.path()));
    Ok(())
}

#[test]
fn something_that_is_not_a_fabric_jar_is_refused_even_with_a_matching_hash() -> TestResult {
    use sha2::{Digest, Sha512};
    let body = b"MZ this is an executable, not a jar".to_vec();
    let f = PlannedFile {
        file_name: "tool.jar".into(),
        url: "https://cdn.modrinth.com/data/AAAAAAAA/versions/a1/tool.jar".into(),
        sha512: hex::encode(Sha512::digest(&body)),
        size: body.len() as u64,
    };
    let t = FakeTransport::new();
    t.cdn(&f.url, &body);
    let dir = tempfile::tempdir()?;
    let err = t.client().download(&f, dir.path()).err();
    assert!(matches!(err, Some(ModError::BadFile { .. })), "{err:?}");
    assert!(nothing_left(dir.path()));
    Ok(())
}

#[test]
fn a_server_error_while_downloading_leaves_nothing_behind() -> TestResult {
    let s = Synth::new("AAAAAAAA", "a1");
    let t = FakeTransport::new();
    t.route(&s.url(), vec![Reply::Status(403)]);
    let dir = tempfile::tempdir()?;
    let err = t.client().download(&planned(&s), dir.path()).err();
    assert!(
        matches!(err, Some(ModError::Http(HttpError::Status(403)))),
        "{err:?}"
    );
    assert!(nothing_left(dir.path()));
    Ok(())
}

#[test]
fn the_user_agent_names_the_program_and_the_contact() {
    let plain = user_agent(None);
    assert!(plain.starts_with("craft-a-server/"));
    assert!(user_agent(Some("me@example.org")).contains("me@example.org"));
    assert!(!user_agent(Some("  ")).contains("; )"));
}
