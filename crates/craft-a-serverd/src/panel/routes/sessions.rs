//! Signed-in screens and session management: home, devices, logout, revoke.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Serialize;

use crate::panel::auth::{AuthSession, clear_cookie};
use crate::panel::error::PanelError;
use crate::panel::middleware::is_htmx;
use crate::panel::pages::{self, DeviceRow, DevicesBody, HomeBody, format_utc};
use crate::panel::{PanelCore, strings};

/// Placeholder home page: proves the session works.
pub async fn home(session: AuthSession) -> Result<Response, PanelError> {
    let body = HomeBody {
        device_name: &session.device_name,
        csrf_token: &session.csrf_token,
    };
    Ok(pages::page(strings::HOME_TITLE, Some(&session), &body)?.into_response())
}

pub async fn devices_page(
    State(core): State<Arc<PanelCore>>,
    session: AuthSession,
) -> Result<Response, PanelError> {
    let now = core.now();
    let rows = core.blocking(move |db| Ok(db.list_sessions(now)?)).await?;
    let devices = rows
        .into_iter()
        .map(|r| DeviceRow {
            current: r.id == session.id,
            id: r.id,
            device_name: r.device_name,
            last_seen: format_utc(r.last_seen_unix),
        })
        .collect();
    let body = DevicesBody {
        devices,
        csrf_token: &session.csrf_token,
    };
    Ok(pages::page(strings::DEVICES_TITLE, Some(&session), &body)?.into_response())
}

#[derive(Debug, Serialize)]
struct SessionView {
    id: String,
    device_name: String,
    created_unix: i64,
    last_seen_unix: i64,
    expires_unix: i64,
    current: bool,
}

/// `GET /api/sessions`: public ids and metadata only, never token hashes or
/// CSRF tokens.
pub async fn list(
    State(core): State<Arc<PanelCore>>,
    session: AuthSession,
) -> Result<Response, PanelError> {
    let now = core.now();
    let rows = core.blocking(move |db| Ok(db.list_sessions(now)?)).await?;
    let views: Vec<SessionView> = rows
        .into_iter()
        .map(|r| SessionView {
            current: r.id == session.id,
            id: r.id,
            device_name: r.device_name,
            created_unix: r.created_unix,
            last_seen_unix: r.last_seen_unix,
            expires_unix: r.expires_unix,
        })
        .collect();
    Ok(Json(views).into_response())
}

fn with_clear_cookie(mut res: Response) -> Response {
    if let Ok(v) = HeaderValue::from_str(&clear_cookie()) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
    res
}

/// What a successful action answers: a redirect for plain HTML form posts,
/// an `HX-Redirect` for htmx, JSON for everyone else.
fn done(headers: &HeaderMap, redirect_to: &'static str) -> Response {
    let is_form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("application/x-www-form-urlencoded")
        });
    if is_htmx(headers) {
        let mut res = Json(serde_json::json!({ "ok": true })).into_response();
        res.headers_mut()
            .insert("hx-redirect", HeaderValue::from_static(redirect_to));
        res
    } else if is_form {
        Redirect::to(redirect_to).into_response()
    } else {
        Json(serde_json::json!({ "ok": true })).into_response()
    }
}

async fn delete_session(core: &PanelCore, id: String) -> Result<(), PanelError> {
    // Ids are 16 hex characters; anything else cannot exist.
    if id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(PanelError::NotFound);
    }
    let removed = core.blocking(move |db| Ok(db.delete_session(&id)?)).await?;
    if removed {
        Ok(())
    } else {
        Err(PanelError::NotFound)
    }
}

/// `POST /api/logout`: ends this phone's session and clears its cookie.
pub async fn logout(
    State(core): State<Arc<PanelCore>>,
    session: AuthSession,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    delete_session(&core, session.id).await?;
    Ok(with_clear_cookie(done(&headers, "/pair")))
}

/// `DELETE /api/sessions/{id}`: signs a device out. Revoking the current
/// device is a logout. Takes effect on the very next request, because every
/// request looks its session up in the database.
pub async fn revoke(
    State(core): State<Arc<PanelCore>>,
    session: AuthSession,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let current = id == session.id;
    delete_session(&core, id).await?;
    if current {
        Ok(with_clear_cookie(done(&headers, "/pair")))
    } else {
        Ok(done(&headers, "/settings/devices"))
    }
}

/// `POST /api/sessions/{id}/revoke`: the same for plain HTML forms.
pub async fn revoke_form(
    State(core): State<Arc<PanelCore>>,
    session: AuthSession,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PanelError> {
    let current = id == session.id;
    delete_session(&core, id).await?;
    if current {
        Ok(with_clear_cookie(done(&headers, "/pair")))
    } else {
        Ok(done(&headers, "/settings/devices"))
    }
}
