//! Online play: the optional playit.gg tunnel.
//!
//! Turning it on goes through a confirm page that states what is shared with
//! playit.gg (server traffic and players' IP addresses pass through a third
//! party); the POST only works with the `confirm=yes` field that page sends.
//! Turning it off needs no confirmation. The claim link and the public
//! address come from the core's validators and are escaped by the templates.

use axum::extract::{Form, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::util::{ConfirmForm, redirect, render};
use crate::lifecycle::StatusProbe;
use crate::panel::error::PanelError;
use crate::panel::mapping::service_error;
use crate::panel::views::{OnlineBody, OnlineEnableBody, OnlineStatusBody, online_status};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;

fn status_html<R, P>(state: &PanelState<R, P>, session: &AuthSession) -> Result<String, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    match state.service().tunnel_manager() {
        None => render(&OnlineStatusBody::unavailable(&session.csrf_token)),
        Some(_) => {
            let info = state
                .service()
                .tunnel_info()
                .map_err(|e| service_error(&e))?;
            render(&online_status(&info, &session.csrf_token))
        }
    }
}

/// `GET /online`
pub async fn page<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let status = status_html(&state, &session)?;
    Ok(pages::screen(
        s::ONLINE_TITLE,
        "online",
        &session,
        &OnlineBody { status: &status },
    )?
    .into_response())
}

/// `GET /partials/online`: polled by the status card.
pub async fn status<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    Ok(Html(status_html(&state, &session)?).into_response())
}

/// `GET /online/enable`: what is shared with playit.gg, and the confirm button.
pub async fn enable_confirm<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    if state.service().tunnel_manager().is_none() {
        return Ok(redirect("/online"));
    }
    let body = OnlineEnableBody {
        csrf_token: &session.csrf_token,
    };
    Ok(pages::screen(s::ONLINE_ENABLE_TITLE, "online", &session, &body)?.into_response())
}

/// `POST /online/enable`: needs the `confirm=yes` field of the confirm page.
pub async fn enable<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<ConfirmForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    form.require()?;
    state
        .service()
        .tunnel_enable(true)
        .map_err(|e| service_error(&e))?;
    Ok(redirect("/online"))
}

#[derive(Debug, Deserialize)]
pub struct DisableForm {
    #[serde(default)]
    forget: Option<String>,
}

/// `POST /online/disable`: `forget=yes` also removes the saved playit.gg link.
pub async fn disable<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<DisableForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    state
        .service()
        .tunnel_disable(form.forget.as_deref() == Some("yes"))
        .await
        .map_err(|e| service_error(&e))?;
    Ok(redirect("/online"))
}
