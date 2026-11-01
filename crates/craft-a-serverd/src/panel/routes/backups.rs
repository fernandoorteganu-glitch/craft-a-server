//! Backups: list, create, and restore behind a confirm step.

use axum::extract::{Form, Path, State};
use axum::response::{Html, IntoResponse, Response};

use super::util::{ConfirmForm, first_server, redirect, render, require_server};
use crate::db::BackupRow;
use crate::lifecycle::{Reason, StatusProbe};
use crate::panel::activity::spawn_action;
use crate::panel::error::PanelError;
use crate::panel::mapping::service_error;
use crate::panel::views::{BackupView, BackupsBody, BackupsList, ConfirmBody, valid_backup_id};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;

/// The backups of the server, newest first (empty without a server).
fn backups_of<R: ContainerRuntime, P: StatusProbe>(
    state: &PanelState<R, P>,
) -> Result<Vec<BackupRow>, PanelError> {
    let Some(server) = first_server(state.service())? else {
        return Ok(Vec::new());
    };
    let mut rows = state
        .service()
        .list_backups(&server.id)
        .map_err(|e| service_error(&e))?;
    rows.sort_by(|a, b| b.created_unix.cmp(&a.created_unix).then(b.id.cmp(&a.id)));
    Ok(rows)
}

fn list_html<R, P>(state: &PanelState<R, P>) -> Result<String, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let backups = backups_of(state)?.iter().map(BackupView::new).collect();
    render(&BackupsList {
        backups,
        banner: state.activity().banner(state.core().now()),
    })
}

pub async fn page<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let list = list_html(&state)?;
    let body = BackupsBody {
        list: &list,
        has_server: first_server(state.service())?.is_some(),
        busy: state.activity().is_busy(),
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(s::BACKUPS_TITLE, "backups", &session, &body)?.into_response())
}

/// `GET /partials/backups`: polled by the list itself.
pub async fn list<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    Ok(Html(list_html(&state)?).into_response())
}

/// `POST /backups/create`: a manual backup, run in the background.
pub async fn create<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let id = require_server(state.service())?.id;
    spawn_action(
        &state,
        s::BUSY_BACKUP,
        s::DONE_BACKUP,
        move |svc| async move { svc.backup(&id, Reason::Manual).await.map(|_| ()) },
    )?;
    Ok(redirect("/backups"))
}

/// Finds one of the server's backups by id; anything else is a plain 404.
fn find_backup<R: ContainerRuntime, P: StatusProbe>(
    state: &PanelState<R, P>,
    backup_id: &str,
) -> Result<BackupRow, PanelError> {
    if !valid_backup_id(backup_id) {
        return Err(PanelError::NotFound);
    }
    backups_of(state)?
        .into_iter()
        .find(|b| b.id == backup_id)
        .ok_or(PanelError::NotFound)
}

/// `GET /backups/{id}/restore`: the confirm step.
pub async fn restore_confirm<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
    Path(backup_id): Path<String>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let backup = find_backup(&state, &backup_id)?;
    let view = BackupView::new(&backup);
    let body = ConfirmBody {
        title: s::RESTORE_TITLE,
        warning: s::RESTORE_WARNING,
        detail: format!("{} ({}, {})", view.created, view.kind, view.size),
        action: format!("/backups/{}/restore", backup.id),
        confirm_label: s::RESTORE_CONFIRM,
        cancel_href: "/backups",
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(s::RESTORE_TITLE, "backups", &session, &body)?.into_response())
}

/// `POST /backups/{id}/restore`: needs the `confirm=yes` field of the confirm page.
pub async fn restore<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Path(backup_id): Path<String>,
    Form(form): Form<ConfirmForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    form.require()?;
    let backup = find_backup(&state, &backup_id)?;
    let server_id = require_server(state.service())?.id;
    spawn_action(
        &state,
        s::BUSY_RESTORE,
        s::DONE_RESTORE,
        move |svc| async move { svc.restore(&server_id, &backup.id).await },
    )?;
    Ok(redirect("/backups"))
}
