//! Players: the allowed list (whitelist) and kick.
//!
//! The only way the panel talks to the game console is the closed
//! [`Console`] set, and every name is validated (`[A-Za-z0-9_]{3,16}`) before
//! a command is even built. No field is ever sent to RCON as free text.

use axum::extract::{Form, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::util::{first_server, redirect, require_server};
use crate::lifecycle::{Console, ServiceError, StatusProbe, player_name};
use crate::panel::error::PanelError;
use crate::panel::mapping::service_error_text;
use crate::panel::views::{PlayersBody, clean_reply, parse_whitelist};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;
use craft_a_server_core::model::ServerState;

pub async fn page<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let server = first_server(state.service())?;
    let running = server
        .as_ref()
        .is_some_and(|row| row.state == ServerState::Running);
    let mut players = Vec::new();
    if let (true, Some(row)) = (running, &server) {
        match state
            .service()
            .console(&row.id, &Console::WhitelistList)
            .await
        {
            Ok(reply) => players = parse_whitelist(&reply),
            Err(e) => tracing::warn!(error = %e, "could not read the whitelist"),
        }
    }
    let body = PlayersBody {
        banner: state.activity().banner(state.core().now()),
        csrf_token: &session.csrf_token,
        running,
        players,
    };
    Ok(pages::screen(s::PLAYERS_TITLE, "players", &session, &body)?.into_response())
}

#[derive(Debug, Deserialize)]
pub struct NameForm {
    name: String,
}

/// Runs one console command and leaves the outcome as a notice.
async fn run<R, P>(
    state: &PanelState<R, P>,
    form: &NameForm,
    command: fn(String) -> Console,
    done: &str,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let now = state.core().now();
    // Validate first so a bad name never reaches the console layer.
    if player_name(&form.name).is_err() {
        state.activity().set_notice(false, s::ERR_PLAYER_NAME, now);
        return Ok(redirect("/players"));
    }
    let id = require_server(state.service())?.id;
    let outcome = state
        .service()
        .console(&id, &command(form.name.clone()))
        .await;
    match outcome {
        Ok(reply) => {
            let said = clean_reply(&reply);
            let text = if said.is_empty() {
                format!("{done}: {}", form.name)
            } else {
                format!("{done}: {}. Server said: {said}", form.name)
            };
            state.activity().set_notice(true, &text, now);
        }
        Err(e) => {
            tracing::warn!(error = %e, "player command failed");
            let text = match e {
                ServiceError::WrongState { .. } => s::ERR_NOT_RUNNING.to_owned(),
                other => service_error_text(&other),
            };
            state.activity().set_notice(false, &text, now);
        }
    }
    Ok(redirect("/players"))
}

pub async fn add<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<NameForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    run(&state, &form, Console::WhitelistAdd, s::PLAYERS_ADDED).await
}

pub async fn remove<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<NameForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    run(&state, &form, Console::WhitelistRemove, s::PLAYERS_REMOVED).await
}

pub async fn kick<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<NameForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    run(&state, &form, Console::Kick, s::PLAYERS_KICKED).await
}
