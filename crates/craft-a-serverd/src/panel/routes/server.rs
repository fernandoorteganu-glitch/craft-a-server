//! Home (the server card), start/stop/restart, reset/delete (with confirm
//! step) and the create-server screen.

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use craft_a_server_core::model::ServerState;
use serde::Deserialize;

use super::util::{ConfirmForm, first_server, redirect, render, require_server};
use crate::lifecycle::StatusProbe;
use crate::panel::activity::spawn_action;
use crate::panel::error::{PanelError, Rendered};
use crate::panel::mapping::{service_error, service_error_text};
use crate::panel::views::{
    CardBody, ConfirmBody, CreateBody, HomeBody, MemoryView, ServerView, format_mib,
};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;

/// The server card as HTML (home page and the htmx poll share it).
async fn card_html<R, P>(
    state: &PanelState<R, P>,
    session: &AuthSession,
) -> Result<String, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let banner = state.activity().banner(state.core().now());
    let service = state.service();
    let server = match first_server(service)? {
        Some(row) => {
            let status = if row.state == ServerState::Running {
                service.live_status(&row.id).await.ok().flatten()
            } else {
                None
            };
            Some(ServerView::new(
                &row,
                status.as_ref(),
                &state.core().config().panel_hostname,
                state.lan_ip(),
                banner.busy.is_some(),
            ))
        }
        None => None,
    };
    // The public address shows on the home card once online play is running.
    let online_address = match service.tunnel_info() {
        Ok(info) if info.enabled => match info.state {
            craft_a_server_core::tunnel::TunnelState::Running { address } => Some(address),
            _ => None,
        },
        _ => None,
    };
    render(&CardBody {
        server,
        banner,
        csrf_token: &session.csrf_token,
        online_address,
    })
}

pub async fn home<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let card = card_html(&state, &session).await?;
    let body = HomeBody {
        card: &card,
        device_name: &session.device_name,
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(s::HOME_TITLE, "home", &session, &body)?.into_response())
}

/// `GET /partials/server-card`: polled by the card itself.
pub async fn card<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    Ok(Html(card_html(&state, &session).await?).into_response())
}

pub async fn start<R, P>(
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
        s::BUSY_START,
        s::DONE_START,
        move |svc| async move { svc.start(&id).await },
    )?;
    Ok(redirect("/"))
}

pub async fn stop<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let id = require_server(state.service())?.id;
    spawn_action(&state, s::BUSY_STOP, s::DONE_STOP, move |svc| async move {
        svc.stop(&id).await
    })?;
    Ok(redirect("/"))
}

pub async fn restart<R, P>(
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
        s::BUSY_RESTART,
        s::DONE_RESTART,
        move |svc| async move { svc.restart(&id).await },
    )?;
    Ok(redirect("/"))
}

/// `GET /server/reset`: the confirm step.
pub async fn reset_confirm<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let Some(row) = first_server(state.service())? else {
        return Ok(redirect("/"));
    };
    let body = ConfirmBody {
        title: s::RESET_TITLE,
        warning: s::RESET_WARNING,
        detail: row.name,
        action: "/server/reset".to_owned(),
        confirm_label: s::RESET_CONFIRM,
        cancel_href: "/",
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(s::RESET_TITLE, "home", &session, &body)?.into_response())
}

/// `POST /server/reset`: needs the `confirm=yes` field of the confirm page.
pub async fn reset<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<ConfirmForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    form.require()?;
    let id = require_server(state.service())?.id;
    spawn_action(
        &state,
        s::BUSY_RESET,
        s::DONE_RESET,
        move |svc| async move { svc.reset(&id).await },
    )?;
    Ok(redirect("/"))
}

/// `GET /server/delete`: the confirm step.
pub async fn delete_confirm<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let Some(row) = first_server(state.service())? else {
        return Ok(redirect("/"));
    };
    let body = ConfirmBody {
        title: s::DELETE_TITLE,
        warning: s::DELETE_WARNING,
        detail: row.name,
        action: "/server/delete".to_owned(),
        confirm_label: s::DELETE_CONFIRM,
        cancel_href: "/",
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(s::DELETE_TITLE, "home", &session, &body)?.into_response())
}

/// `POST /server/delete`: needs the `confirm=yes` field of the confirm page.
pub async fn delete<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<ConfirmForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    form.require()?;
    let id = require_server(state.service())?.id;
    spawn_action(
        &state,
        s::BUSY_DELETE,
        s::DONE_DELETE,
        move |svc| async move { svc.delete_server(&id).await },
    )?;
    Ok(redirect("/"))
}

// ------------------------------------------------------------------ create

fn memory_view<R, P>(state: &PanelState<R, P>) -> Option<MemoryView>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    state.service().memory_plan().ok().map(|plan| MemoryView {
        text: format_mib(plan.heap_mib),
        low: plan.low_memory_warning,
    })
}

fn create_page<R, P>(
    state: &PanelState<R, P>,
    session: &AuthSession,
    error: Option<&str>,
    name: &str,
    mc_version: &str,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let body = CreateBody {
        csrf_token: &session.csrf_token,
        error,
        name,
        mc_version,
        memory: memory_view(state),
    };
    let mut res = pages::screen(s::CREATE_TITLE, "home", session, &body)?.into_response();
    if error.is_some() {
        *res.status_mut() = StatusCode::BAD_REQUEST;
        // Already a friendly page; keep the error shaper from replacing it.
        res.extensions_mut().insert(Rendered);
    }
    Ok(res)
}

/// `GET /create`: one server only, so an existing one sends you home.
pub async fn create_page_get<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    if first_server(state.service())?.is_some() {
        return Ok(redirect("/"));
    }
    create_page(
        &state,
        &session,
        None,
        s::CREATE_NAME_DEFAULT,
        s::CREATE_VERSION_DEFAULT,
    )
}

#[derive(Debug, Deserialize)]
pub struct CreateForm {
    name: String,
    mc_version: String,
    /// The checkbox sends `yes` only when ticked; nothing else counts.
    #[serde(default)]
    eula: Option<String>,
}

/// `POST /server/create`.
pub async fn create<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
    Form(form): Form<CreateForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let service = state.service();
    // The create is quick, so it runs inside the request, but it takes the
    // action slot so two taps cannot create two servers.
    if first_server(service)?.is_some() {
        return Err(PanelError::Conflict(s::ERR_SERVER_EXISTS.to_owned()));
    }
    let guard = state.activity().begin(s::BUSY_CREATE)?;
    // Checked again now that the slot is ours (another tap may have won).
    if first_server(service)?.is_some() {
        return Err(PanelError::Conflict(s::ERR_SERVER_EXISTS.to_owned()));
    }
    let form_error = |message: &str| {
        create_page(
            &state,
            &session,
            Some(message),
            &form.name,
            &form.mc_version,
        )
    };
    if form.eula.as_deref() != Some("yes") {
        return form_error(s::ERR_EULA);
    }
    if service.memory_plan().is_err() {
        return form_error(s::ERR_MEMORY);
    }
    match service
        .create_server(&form.name, &form.mc_version, true)
        .await
    {
        Ok(_) => {
            guard.finish(true, s::DONE_CREATE, state.core().now());
            Ok(redirect("/"))
        }
        Err(e @ crate::lifecycle::ServiceError::InvalidInput(_)) => {
            form_error(&service_error_text(&e))
        }
        Err(e) => Err(service_error(&e)),
    }
}
