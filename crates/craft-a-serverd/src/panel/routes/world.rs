//! World import: upload a zip from the browser, review what it holds, then
//! replace the world (and the files that go with it) behind a confirm step.
//!
//! The upload is the one request in the panel that is big and slow, so the
//! `limits` middleware lets exactly `POST /world/upload` carry a large body
//! for a long time. Everything else about it is as strict as any other
//! state-changing request: session, Origin, and the CSRF token (sent by
//! `static/world.js` in the `X-CSRF-Token` header).

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;

use super::util::{ConfirmForm, first_server, redirect, render, require_server};
use crate::lifecycle::{StatusProbe, WorldImportOutcome};
use crate::panel::activity::spawn_action_with;
use crate::panel::error::PanelError;
use crate::panel::mapping::service_error;
use crate::panel::views::{WorldBody, WorldReviewBody, WorldStatus, format_bytes};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;

/// A client that sends nothing for this long is cut off.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const ZIP_MAGIC: [u8; 4] = [b'P', b'K', 3, 4];
/// The smallest possible zip (an empty archive) is 22 bytes.
const MIN_ZIP_BYTES: u64 = 22;
const MODS_SHOWN: usize = 50;
const NOTES_SHOWN: usize = 3;

fn status_html<R, P>(state: &PanelState<R, P>) -> Result<String, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    render(&WorldStatus {
        banner: state.activity().banner(state.core().now()),
    })
}

/// `GET /world`
pub async fn page<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let server = first_server(state.service())?;
    let (pending, max_bytes) = match &server {
        Some(row) => (
            state.service().world_upload_path(&row.id).is_file(),
            state.service().world_upload_budget(&row.id).unwrap_or(0),
        ),
        None => (false, 0),
    };
    let status = status_html(&state)?;
    let body = WorldBody {
        status: &status,
        has_server: server.is_some(),
        pending,
        busy: state.activity().is_busy(),
        csrf_token: &session.csrf_token,
        max_bytes,
        max_text: format_bytes(max_bytes),
    };
    Ok(pages::screen(s::WORLD_TITLE, "world", &session, &body)?.into_response())
}

/// `GET /partials/world`: polled by the status block.
pub async fn status<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    Ok(Html(status_html(&state)?).into_response())
}

/// Removes a half-written upload unless the upload finished (also when the
/// browser goes away and this request is dropped).
struct PartialFile(Option<PathBuf>);

impl PartialFile {
    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn open_partial(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map(tokio::fs::File::from_std)
}

/// `POST /world/upload`: the zip as the raw request body.
pub async fn upload<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let id = require_server(state.service())?.id;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| PanelError::Invalid(s::ERR_UPLOAD_LENGTH.to_owned()))?;
    if declared < MIN_ZIP_BYTES {
        return Err(PanelError::Invalid(s::ERR_UPLOAD_NOT_ZIP.to_owned()));
    }
    let budget = state
        .service()
        .world_upload_budget(&id)
        .map_err(|e| service_error(&e))?;
    if declared > budget {
        return Err(PanelError::Invalid(format!(
            "That file is too big for the free space here. The limit right now is {}",
            format_bytes(budget)
        )));
    }

    let guard = state.activity().begin(s::BUSY_WORLD_UPLOAD)?;
    let outcome = receive(&state, &id, declared, body).await;
    let now = state.core().now();
    match outcome {
        Ok(()) => {
            guard.finish(true, s::DONE_WORLD_UPLOAD, now);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(e) => {
            state.service().discard_world_upload(&id);
            guard.finish(false, &e.message(), now);
            Err(e)
        }
    }
}

async fn receive<R, P>(
    state: &PanelState<R, P>,
    id: &str,
    declared: u64,
    mut body: Body,
) -> Result<(), PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let service = state.service();
    let partial = service.world_upload_partial_path(id);
    let target = service.world_upload_path(id);
    let mut cleanup = PartialFile(Some(partial.clone()));
    let mut file = open_partial(&partial).map_err(|e| PanelError::internal(&e))?;

    let mut received = 0_u64;
    let mut head: Vec<u8> = Vec::with_capacity(ZIP_MAGIC.len());
    loop {
        let frame = tokio::time::timeout(IDLE_TIMEOUT, body.frame())
            .await
            .map_err(|_| PanelError::Timeout)?;
        let Some(frame) = frame else { break };
        let frame = frame.map_err(|_| PanelError::Invalid(s::ERR_UPLOAD_INCOMPLETE.to_owned()))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        received = received.saturating_add(data.len() as u64);
        if received > declared {
            return Err(PanelError::Invalid(s::ERR_UPLOAD_INCOMPLETE.to_owned()));
        }
        if head.len() < ZIP_MAGIC.len() {
            let missing = ZIP_MAGIC.len() - head.len();
            head.extend(data.iter().take(missing));
            if head.len() == ZIP_MAGIC.len() && head != ZIP_MAGIC {
                return Err(PanelError::Invalid(s::ERR_UPLOAD_NOT_ZIP.to_owned()));
            }
        }
        file.write_all(&data)
            .await
            .map_err(|e| PanelError::internal(&e))?;
    }
    if received != declared || head.len() < ZIP_MAGIC.len() {
        return Err(PanelError::Invalid(s::ERR_UPLOAD_INCOMPLETE.to_owned()));
    }
    file.sync_all()
        .await
        .map_err(|e| PanelError::internal(&e))?;
    drop(file);

    match std::fs::remove_file(&target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(PanelError::internal(&e)),
    }
    std::fs::rename(&partial, &target).map_err(|e| PanelError::internal(&e))?;
    cleanup.keep();

    // Look inside now, so a wrong file is reported while the person is still
    // on the upload form.
    if let Err(e) = service.inspect_world_upload(id).await {
        service.discard_world_upload(id);
        return Err(service_error(&e));
    }
    Ok(())
}

/// `GET /world/import`: what the uploaded file would change.
pub async fn review<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let id = require_server(state.service())?.id;
    if !state.service().world_upload_path(&id).is_file() {
        return Ok(redirect("/world"));
    }
    let plan = match state.service().inspect_world_upload(&id).await {
        Ok(plan) => plan,
        Err(e) => {
            state.service().discard_world_upload(&id);
            return Err(service_error(&e));
        }
    };
    let names = plan.mod_names();
    let body = WorldReviewBody {
        csrf_token: &session.csrf_token,
        world_files: plan.world_files,
        world_size: format_bytes(plan.world_bytes),
        mods: names
            .iter()
            .take(MODS_SHOWN)
            .map(|n| (*n).to_owned())
            .collect(),
        mods_more: names.len().saturating_sub(MODS_SHOWN),
        replaces_mods: plan.replaces_mods,
        config_files: plan.config_files,
        lists: plan.lists.join(", "),
        icon: plan.icon,
        properties: plan.properties,
        ignored: plan.ignored,
    };
    Ok(pages::screen(s::WORLD_REVIEW_TITLE, "world", &session, &body)?.into_response())
}

/// `POST /world/import`: needs the `confirm=yes` field of the review page.
pub async fn import<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<ConfirmForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    form.require()?;
    let server = require_server(state.service())?;
    if !state.service().world_upload_path(&server.id).is_file() {
        return Err(PanelError::Conflict(s::ERR_NO_UPLOAD.to_owned()));
    }
    let id = server.id;
    spawn_action_with(&state, s::BUSY_WORLD_IMPORT, move |svc| async move {
        svc.import_world(&id).await.map(|o| summary(&o))
    })?;
    Ok(redirect("/world"))
}

/// `POST /world/discard`: forget the uploaded file.
pub async fn discard<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let id = require_server(state.service())?.id;
    state.service().discard_world_upload(&id);
    state
        .activity()
        .set_notice(true, s::DONE_WORLD_DISCARD, state.core().now());
    Ok(redirect("/world"))
}

/// The sentence shown after an import. Holds only counts and file names that
/// were made safe on import, and the template escapes it.
fn summary(outcome: &WorldImportOutcome) -> String {
    let report = &outcome.report;
    let mut text = if outcome.verified {
        String::from("World imported. The server is running with the new files.")
    } else {
        String::from("World imported. Start the server from Home to play.")
    };
    let mut parts = Vec::new();
    if !report.mods_imported.is_empty() {
        parts.push(format!("{} mods", report.mods_imported.len()));
    }
    if report.config_files > 0 {
        parts.push(format!("{} mod settings files", report.config_files));
    }
    if !report.lists.is_empty() {
        parts.push("player lists".to_owned());
    }
    if report.icon {
        parts.push("the server icon".to_owned());
    }
    if !report.properties_applied.is_empty() {
        parts.push(format!(
            "{} gameplay settings",
            report.properties_applied.len()
        ));
    }
    if !parts.is_empty() {
        text.push_str(" Also imported: ");
        text.push_str(&parts.join(", "));
        text.push('.');
    }
    for note in report.notes.iter().take(NOTES_SHOWN) {
        text.push(' ');
        text.push_str(note);
    }
    if report.notes.len() > NOTES_SHOWN {
        text.push_str(&format!(
            " And {} more skipped.",
            report.notes.len() - NOTES_SHOWN
        ));
    }
    text
}
