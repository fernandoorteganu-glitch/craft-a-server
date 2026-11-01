//! The pair page and `POST /api/pair`.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::panel::auth::{
    clean_device_name, device_name_from_user_agent, redeem_code, session_cookie,
};
use crate::panel::error::{PanelError, Rendered};
use crate::panel::middleware::PeerIp;
use crate::panel::pages;
use crate::panel::ratelimit::FailureEffect;
use crate::panel::{MAX_BODY_BYTES, PanelCore};

pub async fn page() -> Result<Response, PanelError> {
    Ok(pages::pair_page(None)?.into_response())
}

#[derive(Debug, Deserialize)]
struct PairInput {
    code: String,
    #[serde(default)]
    device_name: Option<String>,
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("application/json")
        })
}

/// A failure: JSON for API clients, the pair page with a message for forms.
fn failure(json: bool, err: PanelError) -> Response {
    if json {
        return err.into_response();
    }
    match pages::pair_page(Some(&err.message())) {
        Ok(html) => {
            let mut res = html.into_response();
            *res.status_mut() = err.status();
            if let PanelError::RateLimited { retry_after_secs } = err
                && let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string())
            {
                res.headers_mut().insert(header::RETRY_AFTER, v);
            }
            res.extensions_mut().insert(Rendered);
            res
        }
        Err(e) => e.into_response(),
    }
}

pub async fn submit(
    State(core): State<Arc<PanelCore>>,
    Extension(PeerIp(peer)): Extension<PeerIp>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let json = is_json(&parts.headers);
    let now = core.now();

    // Count every attempt, well-formed or not, before doing any work.
    let allowed = core.limiter().check(peer, now);
    if let Err(refused) = allowed {
        return failure(
            json,
            PanelError::RateLimited {
                retry_after_secs: refused.retry_after_secs,
            },
        );
    }

    let Ok(bytes) = axum::body::to_bytes(Body::new(body), MAX_BODY_BYTES).await else {
        return failure(json, PanelError::PayloadTooLarge);
    };
    let parsed: Result<PairInput, ()> = if json {
        serde_json::from_slice(&bytes).map_err(|_| ())
    } else {
        serde_urlencoded::from_bytes(&bytes).map_err(|_| ())
    };
    let Ok(input) = parsed else {
        return failure(json, PanelError::BadRequest);
    };

    let device_name = input
        .device_name
        .as_deref()
        .and_then(clean_device_name)
        .unwrap_or_else(|| {
            let ua = parts
                .headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok());
            device_name_from_user_agent(ua).to_owned()
        });

    let ttl = core.config().session_ttl_secs;
    let typed = input.code;
    let name = device_name.clone();
    let outcome = core
        .blocking(move |db| redeem_code(db, &typed, &name, now, ttl))
        .await;
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return failure(json, e),
    };

    let Some(outcome) = outcome else {
        let effect = core.limiter().record_failure(now);
        if effect == FailureEffect::GlobalLockout {
            tracing::warn!(
                "too many wrong pairing codes: all pending codes were invalidated and pairing is locked for 10 minutes"
            );
            let _ = core.blocking(|db| Ok(db.clear_pairing_codes()?)).await;
        }
        return failure(json, PanelError::PairInvalid);
    };

    core.limiter().record_success(peer);
    tracing::info!(session = %outcome.session.id, device = %outcome.session.device_name, "phone paired");
    let cookie = session_cookie(&outcome.token, ttl);
    let Ok(cookie) = HeaderValue::from_str(&cookie) else {
        return PanelError::internal(&"cookie value was not a valid header").into_response();
    };
    let mut res = if json {
        Json(serde_json::json!({
            "ok": true,
            "device_name": outcome.session.device_name,
            "csrf_token": outcome.session.csrf_token,
        }))
        .into_response()
    } else {
        Redirect::to("/").into_response()
    };
    res.headers_mut().append(header::SET_COOKIE, cookie);
    res
}
