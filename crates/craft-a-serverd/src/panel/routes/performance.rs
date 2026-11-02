//! Performance: what the server and the machine use right now, and the view
//! distance. The view distance is a closed list of numbers; the form value is
//! parsed and range-checked here and again by the service.

use axum::extract::{Form, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::util::{first_server, redirect, render};
use crate::lifecycle::StatusProbe;
use crate::panel::error::PanelError;
use crate::panel::mapping::service_error;
use crate::panel::perf_views::{DistanceBody, PerformanceBody, UsageBody};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;

async fn usage_html<R, P>(state: &PanelState<R, P>) -> Result<String, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let service = state.service();
    match first_server(service)? {
        None => render(&UsageBody::without_server()),
        Some(row) => {
            let resources = service
                .resources(&row.id)
                .await
                .map_err(|e| service_error(&e))?;
            render(&UsageBody::from_resources(&resources))
        }
    }
}

fn distance_html<R, P>(
    state: &PanelState<R, P>,
    session: &AuthSession,
) -> Result<String, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let service = state.service();
    let banner = state.activity().banner(state.core().now());
    match first_server(service)? {
        None => render(&DistanceBody::without_server(&session.csrf_token, banner)),
        Some(row) => {
            let current = service
                .view_distance(&row.id)
                .map_err(|e| service_error(&e))?;
            render(&DistanceBody::new(current, &session.csrf_token, banner))
        }
    }
}

/// `GET /performance`
pub async fn page<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let usage = usage_html(&state).await?;
    let distance = distance_html(&state, &session)?;
    let body = PerformanceBody {
        usage: &usage,
        distance: &distance,
    };
    Ok(pages::screen(s::PERF_TITLE, "performance", &session, &body)?.into_response())
}

/// `GET /partials/performance`: polled by the usage card.
pub async fn usage<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    Ok(Html(usage_html(&state).await?).into_response())
}

#[derive(Debug, Deserialize)]
pub struct ViewDistanceForm {
    #[serde(default)]
    view_distance: String,
}

/// A whole number; anything else is refused (the range is checked by the service).
fn parse_choice(text: &str) -> Result<u8, PanelError> {
    text.trim()
        .parse::<u8>()
        .map_err(|_| PanelError::Invalid(s::ERR_VIEW_DISTANCE.to_owned()))
}

/// `POST /performance/view-distance`
pub async fn set_view_distance<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
    Form(form): Form<ViewDistanceForm>,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let value = parse_choice(&form.view_distance)?;
    let row = super::util::require_server(state.service())?;
    state
        .service()
        .set_view_distance(&row.id, value)
        .map_err(|e| service_error(&e))?;
    state
        .activity()
        .set_notice(true, s::DONE_VIEW_DISTANCE, state.core().now());
    Ok(redirect("/performance"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_value_is_a_small_whole_number() {
        assert_eq!(parse_choice(" 8 ").ok(), Some(8));
        for bad in ["", "default", "-1", "256", "8.5", "eight", "8; drop", "0x8"] {
            assert!(parse_choice(bad).is_err(), "accepted {bad:?}");
        }
    }
}
