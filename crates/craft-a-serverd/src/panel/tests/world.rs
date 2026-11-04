// Test harness code: a failed setup should abort the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

//! Tests of the World screen: the upload (a big raw body behind the same
//! session, Origin and CSRF checks as everything else), the review page, the
//! confirm step and the import itself.

use std::io::Write;
use std::time::Duration;

use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use super::*;

/// A minimal gzip header: all the import checks on a world's level.dat.
const LEVEL_DAT: &[u8] = &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1, 2, 3];

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

async fn create(h: &Harness, s: &Sess) -> String {
    let res = post(
        h,
        s,
        "/server/create",
        "name=Test+world&mc_version=26.3&eula=yes",
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    h.svc
        .list_servers()
        .expect("list")
        .into_iter()
        .next()
        .expect("a server")
        .id
}

fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, data) in files {
        writer.start_file(*name, options).expect("start file");
        writer.write_all(data).expect("write");
    }
    writer.finish().expect("finish").into_inner()
}

/// A world zip bigger than the 16 KiB every other request may carry.
fn big_world() -> Vec<u8> {
    zip_of(&[
        ("world/level.dat", LEVEL_DAT),
        ("world/region/r.0.0.mca", &vec![7_u8; 100 * 1024]),
        ("world/playerdata/a.dat", b"player"),
        ("whitelist.json", b"[{\"name\":\"friend\"}]"),
        ("server.properties", b"difficulty=hard\nonline-mode=false\n"),
        ("logs/latest.log", b"ignored"),
    ])
}

fn upload_request(s: &Sess, body: &[u8]) -> Rb {
    upload_with(s, body, Some(&body.len().to_string()), Some(&s.csrf))
}

/// An upload with a chosen `Content-Length` and CSRF header (or none).
fn upload_with(s: &Sess, body: &[u8], length: Option<&str>, csrf: Option<&str>) -> Rb {
    let mut rb = Rb::post("/world/upload")
        .cookie(&s.token)
        .hdr("content-type", "application/zip")
        .hdr("accept", "application/json")
        .body(body);
    if let Some(length) = length {
        rb = rb.hdr("content-length", length);
    }
    if let Some(csrf) = csrf {
        rb = rb.csrf(csrf);
    }
    rb
}

fn assert_no_inline_code(page: &str) {
    let body = page
        .replace("<script src=\"/static/htmx.min.js\" defer></script>", "")
        .replace("<script src=\"/static/world.js\" defer></script>", "");
    for needle in ["<script", " style=", " onclick=", " onload=", "javascript:"] {
        assert!(!body.contains(needle), "found {needle} in {body}");
    }
}

#[cfg(unix)]
async fn idle(h: &Harness, s: &Sess) {
    for _ in 0..1000 {
        if !get(h, s, "/partials/world")
            .await
            .text()
            .contains("notice-busy")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the background action never finished");
}

#[tokio::test]
async fn the_screen_is_in_the_navigation_and_needs_a_server() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let res = get(&h, &s, "/world").await;
    assert_eq!(res.status, StatusCode::OK);
    let page = res.text();
    assert!(
        page.contains("href=\"/world\" aria-current=\"page\""),
        "{page}"
    );
    assert!(page.contains("You have no server yet"), "{page}");
    assert!(!page.contains("id=\"world-upload\""));
    assert_no_inline_code(&page);
    assert!(get(&h, &s, "/").await.text().contains("href=\"/world\""));
    Ok(())
}

#[tokio::test]
async fn the_upload_form_carries_the_token_and_the_script_is_served() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let page = get(&h, &s, "/world").await.text();
    assert!(page.contains("id=\"world-upload\""), "{page}");
    assert!(
        page.contains(&format!("data-csrf=\"{}\"", s.csrf)),
        "{page}"
    );
    assert!(page.contains("data-max=\""), "{page}");
    assert_no_inline_code(&page);
    let js = h.send(Rb::get("/static/world.js")).await;
    assert_eq!(js.status, StatusCode::OK);
    assert!(
        js.header("content-type")
            .is_some_and(|t| t.starts_with("text/javascript"))
    );
    assert!(js.text().contains("X-CSRF-Token"));
    Ok(())
}

#[tokio::test]
async fn a_big_upload_passes_the_body_limit_and_is_reviewed() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    let body = big_world();
    assert!(body.len() > crate::panel::MAX_BODY_BYTES);

    let res = h.send(upload_request(&s, &body)).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.text());
    assert_eq!(std::fs::read(h.svc.world_upload_path(&id))?, body);
    assert!(!h.svc.world_upload_partial_path(&id).exists());

    let page = get(&h, &s, "/world").await.text();
    assert!(page.contains("already waiting"), "{page}");
    let review = get(&h, &s, "/world/import").await;
    assert_eq!(review.status, StatusCode::OK);
    let text = review.text();
    assert!(text.contains("Review the import"), "{text}");
    assert!(text.contains("3 files"), "{text}");
    assert!(text.contains("whitelist.json"), "{text}");
    assert!(text.contains("name=\"confirm\" value=\"yes\""), "{text}");
    assert_no_inline_code(&text);
    Ok(())
}

#[tokio::test]
async fn other_requests_still_have_the_small_body_limit() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let res = h
        .send(
            Rb::post("/world/discard")
                .cookie(&s.token)
                .csrf(&s.csrf)
                .hdr("content-length", "1000000")
                .body(b"x"),
        )
        .await;
    assert_eq!(res.status, StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

#[tokio::test]
async fn the_upload_needs_a_session_the_csrf_token_and_the_right_origin() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let body = big_world();
    let len = body.len().to_string();

    let anon = h
        .send(
            Rb::post("/world/upload")
                .csrf(&s.csrf)
                .hdr("content-length", &len)
                .body(&body),
        )
        .await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);

    let no_token = h.send(upload_with(&s, &body, Some(&len), None)).await;
    assert_eq!(no_token.status, StatusCode::FORBIDDEN);

    let wrong_token = h
        .send(upload_with(&s, &body, Some(&len), Some("nope")))
        .await;
    assert_eq!(wrong_token.status, StatusCode::FORBIDDEN);

    let other_site = h
        .send(upload_request(&s, &body).origin("http://evil.example"))
        .await;
    assert_eq!(other_site.status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_file_that_is_not_a_zip_is_refused_and_leaves_nothing() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;

    let junk = vec![b'x'; 5000];
    let res = h.send(upload_request(&s, &junk)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
    assert!(res.err_message().contains("not a zip"), "{}", res.text());

    // A zip that holds no world is refused once it has been read.
    let no_world = zip_of(&[("readme.txt", b"hello hello hello")]);
    let res = h.send(upload_request(&s, &no_world)).await;
    assert!(res.status.is_client_error(), "{}", res.text());
    assert!(res.err_message().contains("level.dat"), "{}", res.text());

    assert!(!h.svc.world_upload_path(&id).exists());
    assert!(!h.svc.world_upload_partial_path(&id).exists());
    // The slot is free again.
    let ok = h.send(upload_request(&s, &big_world())).await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.text());
    Ok(())
}

#[tokio::test]
async fn the_declared_length_must_match_and_be_there() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    let body = big_world();

    let no_length = h.send(upload_with(&s, &body, None, Some(&s.csrf))).await;
    assert!(no_length.status.is_client_error(), "{}", no_length.text());

    let short = h
        .send(upload_with(&s, &body, Some("99999999"), Some(&s.csrf)))
        .await;
    assert!(short.status.is_client_error(), "{}", short.text());

    assert!(!h.svc.world_upload_path(&id).exists());
    assert!(!h.svc.world_upload_partial_path(&id).exists());
    Ok(())
}

#[tokio::test]
async fn an_upload_needs_a_server() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let res = h.send(upload_request(&s, &big_world())).await;
    assert!(res.status.is_client_error(), "{}", res.text());
    Ok(())
}

#[tokio::test]
async fn too_little_free_space_refuses_the_upload_before_it_starts() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let body = big_world();
    let res = h
        .send(upload_with(
            &s,
            &body,
            Some(&(200_u64 << 30).to_string()),
            Some(&s.csrf),
        ))
        .await;
    assert!(res.status.is_client_error(), "{}", res.text());
    assert!(res.err_message().contains("too big"), "{}", res.text());
    Ok(())
}

#[tokio::test]
async fn discarding_deletes_the_upload() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    assert_eq!(
        h.send(upload_request(&s, &big_world())).await.status,
        StatusCode::NO_CONTENT
    );
    let res = post(&h, &s, "/world/discard", "").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER);
    assert!(!h.svc.world_upload_path(&id).exists());
    assert!(get(&h, &s, "/world").await.text().contains("deleted"));
    // With nothing waiting, the review page sends you back.
    let review = get(&h, &s, "/world/import").await;
    assert_eq!(review.status, StatusCode::SEE_OTHER);
    Ok(())
}

#[tokio::test]
async fn importing_needs_the_confirm_step() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    assert_eq!(
        h.send(upload_request(&s, &big_world())).await.status,
        StatusCode::NO_CONTENT
    );
    let res = post(&h, &s, "/world/import", "").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
    assert!(h.svc.world_upload_path(&id).exists(), "the upload is kept");
    Ok(())
}

#[tokio::test]
async fn importing_without_an_upload_says_so() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    create(&h, &s).await;
    let res = post(&h, &s, "/world/import", "confirm=yes").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.text());
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn confirming_replaces_the_world_and_reports_what_happened() -> TestResult {
    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    let data = h.svc.data_dir(&id);
    std::fs::create_dir_all(data.join("world"))?;
    std::fs::write(data.join("world/level.dat"), LEVEL_DAT)?;
    std::fs::write(data.join("world/old-only.txt"), "old")?;
    std::fs::write(
        data.join("server.properties"),
        "online-mode=true\ndifficulty=easy\n",
    )?;
    assert_eq!(
        h.send(upload_request(&s, &big_world())).await.status,
        StatusCode::NO_CONTENT
    );

    let res = post(&h, &s, "/world/import", "confirm=yes").await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    idle(&h, &s).await;

    let page = get(&h, &s, "/world").await.text();
    assert!(page.contains("World imported"), "{page}");
    assert!(page.contains("Start the server from Home"), "{page}");
    assert!(data.join("world/region/r.0.0.mca").is_file());
    assert!(!data.join("world/old-only.txt").exists());
    let props = std::fs::read_to_string(data.join("server.properties"))?;
    assert!(
        props.contains("difficulty=hard") && props.contains("online-mode=true"),
        "{props}"
    );
    assert_eq!(
        std::fs::read_to_string(data.join("whitelist.json"))?,
        "[{\"name\":\"friend\"}]"
    );
    assert!(
        !h.svc.world_upload_path(&id).exists(),
        "the upload is deleted"
    );
    assert_no_inline_code(&page);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_import_shows_a_plain_message_without_paths() -> TestResult {
    let (h, dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    // Passes the upload check, fails when read in full: level.dat is not gzip.
    let bad = zip_of(&[("world/level.dat", b"plain text, not gzip")]);
    assert_eq!(
        h.send(upload_request(&s, &bad)).await.status,
        StatusCode::NO_CONTENT
    );

    post(&h, &s, "/world/import", "confirm=yes").await;
    idle(&h, &s).await;

    let page = get(&h, &s, "/world").await.text();
    assert!(
        page.contains("level.dat is not a valid Minecraft file"),
        "{page}"
    );
    let root = dir.path().to_string_lossy().into_owned();
    assert!(!page.contains(&root), "a path leaked: {page}");
    assert!(!h.svc.world_upload_path(&id).exists());
    Ok(())
}

// ---- over a real TCP connection ----------------------------------------------

/// Serves the panel on a real socket, so the upload goes through hyper's body
/// handling and not only through `oneshot`.
async fn serve(h: &Harness) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = h
        .app
        .clone()
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

fn raw_upload_head(s: &Sess, length: usize) -> String {
    format!(
        "POST /world/upload HTTP/1.1\r\nHost: {HOST}\r\nOrigin: {ORIGIN}\r\n\
         Cookie: craft_session={}\r\nX-CSRF-Token: {}\r\nAccept: application/json\r\n\
         Content-Type: application/zip\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
        s.token, s.csrf
    )
}

#[tokio::test]
async fn a_large_upload_streams_over_a_real_connection() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    let addr = serve(&h).await;
    let body = zip_of(&[
        ("world/level.dat", LEVEL_DAT),
        ("world/region/r.0.0.mca", &vec![3_u8; 12 * 1024 * 1024]),
    ]);

    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream
        .write_all(raw_upload_head(&s, body.len()).as_bytes())
        .await?;
    for chunk in body.chunks(64 * 1024) {
        stream.write_all(chunk).await?;
    }
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await?;

    assert!(answer.starts_with("HTTP/1.1 204"), "{answer}");
    assert_eq!(std::fs::read(h.svc.world_upload_path(&id))?, body);
    assert!(!h.svc.world_upload_partial_path(&id).exists());
    Ok(())
}

#[tokio::test]
async fn a_connection_that_drops_mid_upload_leaves_no_file_and_frees_the_slot() -> TestResult {
    use tokio::io::AsyncWriteExt;

    let (h, _dir) = private_harness();
    let s = h.pair("Pixel").await;
    let id = create(&h, &s).await;
    let addr = serve(&h).await;
    let body = zip_of(&[
        ("world/level.dat", LEVEL_DAT),
        ("world/region/r.0.0.mca", &vec![3_u8; 2 * 1024 * 1024]),
    ]);

    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream
        .write_all(raw_upload_head(&s, body.len()).as_bytes())
        .await?;
    stream
        .write_all(body.get(..body.len() / 2).unwrap_or_default())
        .await?;
    // Wait until the daemon has started writing, then vanish.
    for _ in 0..500 {
        if h.svc.world_upload_partial_path(&id).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        h.svc.world_upload_partial_path(&id).exists(),
        "the upload never started"
    );
    drop(stream);

    for _ in 0..500 {
        if !h.svc.world_upload_partial_path(&id).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !h.svc.world_upload_partial_path(&id).exists(),
        "a half upload was left behind"
    );
    assert!(!h.svc.world_upload_path(&id).exists());
    // A new upload is accepted again.
    let ok = h.send(upload_request(&s, &big_world())).await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.text());
    Ok(())
}
