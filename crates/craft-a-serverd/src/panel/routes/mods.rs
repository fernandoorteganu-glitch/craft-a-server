//! Mods: search Modrinth, install with a confirm step, remove, update check.
//!
//! Everything from Modrinth (names, descriptions, authors) is attacker
//! controlled: the service cleans it and the templates escape it. Icons are
//! not shown: the CSP allows images from this origin only, and it is not
//! loosened for a CDN. Installs and removals run in the background slot and
//! the screen polls the installed list for progress.

use axum::extract::{Form, Path, RawQuery, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::util::{first_server, redirect, render, require_server};
use crate::lifecycle::StatusProbe;
use crate::mods::model::valid_id;
use crate::panel::activity::spawn_action;
use crate::panel::error::PanelError;
use crate::panel::mapping::{service_error, service_error_text};
use crate::panel::views::{
    InstalledView, ModHitView, ModInstallBody, ModRemoveBody, ModUpdatesBody, ModsBody, ModsList,
    format_bytes,
};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;

#[derive(Debug, Default, Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    unknown: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PlanParams {
    #[serde(default)]
    pre: Option<String>,
}

fn parse<T: Default + for<'de> Deserialize<'de>>(raw: Option<&str>) -> T {
    raw.and_then(|r| serde_urlencoded::from_str(r).ok())
        .unwrap_or_default()
}

/// Changes run only after the confirm page was passed.
fn need_confirm(confirm: Option<&str>) -> Result<(), PanelError> {
    if confirm == Some("yes") {
        Ok(())
    } else {
        Err(PanelError::Invalid(s::ERR_CONFIRM.to_owned()))
    }
}

/// A project id from the path; anything else is a plain 404.
fn project_id(id: &str) -> Result<&str, PanelError> {
    if valid_id(id) {
        Ok(id)
    } else {
        Err(PanelError::NotFound)
    }
}

async fn list_html<R, P>(state: &PanelState<R, P>) -> Result<String, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let (mods, untracked) = match first_server(state.service())? {
        Some(row) => {
            let list = state
                .service()
                .mods_list(&row.id)
                .await
                .map_err(|e| service_error(&e))?;
            (
                list.mods.iter().map(InstalledView::new).collect(),
                list.untracked,
            )
        }
        None => (Vec::new(), Vec::new()),
    };
    render(&ModsList {
        mods,
        untracked,
        banner: state.activity().banner(state.core().now()),
    })
}

pub async fn page<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
    RawQuery(raw): RawQuery,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let params: SearchParams = parse(raw.as_deref());
    let server = first_server(state.service())?;
    let include_unknown = params.unknown.as_deref() == Some("1");
    let query: String = params
        .q
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();

    let mut results = None;
    let mut search_error = None;
    if let (Some(row), true) = (&server, !query.trim().is_empty()) {
        match state
            .service()
            .mods_search(&row.id, &query, include_unknown, 0)
            .await
        {
            Ok(hits) => results = Some(hits.iter().map(ModHitView::new).collect()),
            Err(e) => {
                tracing::warn!(error = %e, "mod search failed");
                search_error = Some(service_error_text(&e));
            }
        }
    }
    let list = list_html(&state).await?;
    let body = ModsBody {
        has_server: server.is_some(),
        busy: state.activity().is_busy(),
        query: &query,
        include_unknown,
        results,
        search_error,
        list: &list,
    };
    Ok(pages::screen(s::MODS_TITLE, "mods", &session, &body)?.into_response())
}

/// `GET /partials/mods`: polled by the installed list for progress.
pub async fn list<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    Ok(Html(list_html(&state).await?).into_response())
}

/// `GET /mods/updates`: read-only comparison with Modrinth.
pub async fn updates<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let server = require_server(state.service())?;
    let (updates, error) = match state.service().mods_check(&server.id).await {
        Ok(u) => (u, None),
        Err(e) => {
            tracing::warn!(error = %e, "mod update check failed");
            (Vec::new(), Some(service_error_text(&e)))
        }
    };
    let body = ModUpdatesBody {
        updates,
        error,
        busy: state.activity().is_busy(),
    };
    Ok(pages::screen(s::MODS_UPDATES_TITLE, "mods", &session, &body)?.into_response())
}

/// `GET /mods/install/{id}`: what will be installed, and the confirm button.
pub async fn install_confirm<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
    Path(id): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let project = project_id(&id)?;
    let prerelease = parse::<PlanParams>(raw.as_deref()).pre.as_deref() == Some("1");
    let server = require_server(state.service())?;
    let plan = state
        .service()
        .mods_plan(&server.id, project, prerelease)
        .await
        .map_err(|e| service_error(&e))?;
    let title = if plan.replaces.is_some() {
        s::MODS_UPDATE_TITLE
    } else {
        s::MODS_INSTALL_TITLE
    };
    let body = ModInstallBody {
        title,
        size: format_bytes(plan.target.size_bytes),
        already: plan.already_installed.join(", "),
        optional: plan.optional.join(", "),
        action: format!("/mods/install/{project}"),
        prerelease,
        plan,
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(title, "mods", &session, &body)?.into_response())
}

#[derive(Debug, Deserialize)]
pub struct InstallForm {
    #[serde(default)]
    confirm: Option<String>,
    #[serde(default)]
    pre: Option<String>,
}

/// `POST /mods/install/{id}`: runs the transaction in the background.
pub async fn install<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Path(id): Path<String>,
    Form(form): Form<InstallForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    need_confirm(form.confirm.as_deref())?;
    let project = project_id(&id)?.to_owned();
    let server_id = require_server(state.service())?.id;
    let prerelease = form.pre.as_deref() == Some("1");
    spawn_action(
        &state,
        s::BUSY_MODS_INSTALL,
        s::DONE_MODS_INSTALL,
        move |svc| async move {
            svc.mods_install(&server_id, &project, prerelease)
                .await
                .map(|_| ())
        },
    )?;
    Ok(redirect("/mods"))
}

/// `GET /mods/remove/{id}`: dependents and orphans, then the confirm button.
pub async fn remove_confirm<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
    Path(id): Path<String>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let project = project_id(&id)?;
    let server = require_server(state.service())?;
    let plan = state
        .service()
        .mods_remove_plan(&server.id, project)
        .map_err(|e| service_error(&e))?;
    let body = ModRemoveBody {
        dependents: plan.dependents.join(", "),
        orphans: plan.orphans.join(", "),
        action: format!("/mods/remove/{project}"),
        plan,
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(s::MODS_REMOVE_TITLE, "mods", &session, &body)?.into_response())
}

#[derive(Debug, Deserialize)]
pub struct RemoveForm {
    #[serde(default)]
    confirm: Option<String>,
    #[serde(default)]
    force: Option<String>,
    #[serde(default)]
    orphans: Option<String>,
}

/// `POST /mods/remove/{id}`.
pub async fn remove<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Path(id): Path<String>,
    Form(form): Form<RemoveForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    need_confirm(form.confirm.as_deref())?;
    let project = project_id(&id)?.to_owned();
    let server_id = require_server(state.service())?.id;
    let (force, orphans) = (
        form.force.as_deref() == Some("yes"),
        form.orphans.as_deref() == Some("yes"),
    );
    spawn_action(
        &state,
        s::BUSY_MODS_REMOVE,
        s::DONE_MODS_REMOVE,
        move |svc| async move {
            svc.mods_remove(&server_id, &project, force, orphans)
                .await
                .map(|_| ())
        },
    )?;
    Ok(redirect("/mods"))
}
