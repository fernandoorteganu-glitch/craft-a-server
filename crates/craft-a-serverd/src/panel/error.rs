//! The panel's single error type.
//!
//! Handlers and middleware return [`PanelError`]; it becomes a response whose
//! body is produced centrally (see `middleware::shape_errors`) as a friendly
//! HTML page or a JSON object depending on the request. Messages come from
//! `strings.rs` and never contain paths, SQL or other internals; the cause of
//! an internal failure is logged where it happens.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::strings as s;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PanelError {
    #[error("not signed in")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("csrf token missing or wrong")]
    CsrfFailed,
    #[error("origin check failed")]
    OriginRejected,
    #[error("host not allowed")]
    HostRejected,
    #[error("invalid pairing code")]
    PairInvalid,
    #[error("rate limited")]
    RateLimited { retry_after_secs: u64 },
    #[error("not found")]
    NotFound,
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("bad request")]
    BadRequest,
    /// The input was understood but is not acceptable (a screen's own,
    /// already friendly, message).
    #[error("invalid input")]
    Invalid(String),
    /// The action cannot happen in the current state (a friendly message).
    #[error("conflict")]
    Conflict(String),
    #[error("timeout")]
    Timeout,
    #[error("busy")]
    Busy,
    #[error("internal error")]
    Internal,
}

impl PanelError {
    /// Logs the real cause (never shown to the client) and returns `Internal`.
    pub fn internal(cause: &dyn std::fmt::Display) -> PanelError {
        tracing::error!(error = %cause, "panel internal error");
        PanelError::Internal
    }

    pub fn status(&self) -> StatusCode {
        match self {
            PanelError::Unauthorized => StatusCode::UNAUTHORIZED,
            PanelError::Forbidden | PanelError::CsrfFailed | PanelError::OriginRejected => {
                StatusCode::FORBIDDEN
            }
            PanelError::HostRejected => StatusCode::MISDIRECTED_REQUEST,
            PanelError::PairInvalid | PanelError::BadRequest | PanelError::Invalid(_) => {
                StatusCode::BAD_REQUEST
            }
            PanelError::Conflict(_) => StatusCode::CONFLICT,
            PanelError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            PanelError::NotFound => StatusCode::NOT_FOUND,
            PanelError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            PanelError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            PanelError::Timeout => StatusCode::REQUEST_TIMEOUT,
            PanelError::Busy => StatusCode::SERVICE_UNAVAILABLE,
            PanelError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable machine-readable code for JSON clients.
    pub fn code(&self) -> &'static str {
        match self {
            PanelError::Unauthorized => "unauthorized",
            PanelError::Forbidden => "forbidden",
            PanelError::CsrfFailed => "csrf_failed",
            PanelError::OriginRejected => "origin_rejected",
            PanelError::HostRejected => "host_rejected",
            PanelError::PairInvalid => "pair_invalid",
            PanelError::RateLimited { .. } => "rate_limited",
            PanelError::NotFound => "not_found",
            PanelError::MethodNotAllowed => "method_not_allowed",
            PanelError::PayloadTooLarge => "payload_too_large",
            PanelError::BadRequest => "bad_request",
            PanelError::Invalid(_) => "invalid_input",
            PanelError::Conflict(_) => "conflict",
            PanelError::Timeout => "timeout",
            PanelError::Busy => "busy",
            PanelError::Internal => "internal",
        }
    }

    /// Friendly English message (safe to show).
    pub fn message(&self) -> String {
        match self {
            PanelError::Unauthorized => s::ERR_UNAUTHORIZED.to_owned(),
            PanelError::Forbidden => s::ERR_FORBIDDEN.to_owned(),
            PanelError::CsrfFailed => s::ERR_CSRF.to_owned(),
            PanelError::OriginRejected => s::ERR_ORIGIN.to_owned(),
            PanelError::HostRejected => s::ERR_HOST.to_owned(),
            PanelError::PairInvalid => s::ERR_PAIR_INVALID.to_owned(),
            PanelError::RateLimited { retry_after_secs } => {
                format!(
                    "{}. Try again in {retry_after_secs} seconds",
                    s::ERR_RATE_LIMITED
                )
            }
            PanelError::NotFound => s::ERR_NOT_FOUND.to_owned(),
            PanelError::MethodNotAllowed => s::ERR_METHOD.to_owned(),
            PanelError::PayloadTooLarge => s::ERR_TOO_LARGE.to_owned(),
            PanelError::BadRequest => s::ERR_BAD_REQUEST.to_owned(),
            PanelError::Invalid(message) | PanelError::Conflict(message) => message.clone(),
            PanelError::Timeout => s::ERR_TIMEOUT.to_owned(),
            PanelError::Busy => s::ERR_BUSY.to_owned(),
            PanelError::Internal => s::ERR_INTERNAL.to_owned(),
        }
    }

    /// The error for a bare status produced by axum itself (404, 405, 413...).
    pub fn from_status(status: StatusCode) -> PanelError {
        match status {
            StatusCode::NOT_FOUND => PanelError::NotFound,
            StatusCode::METHOD_NOT_ALLOWED => PanelError::MethodNotAllowed,
            StatusCode::PAYLOAD_TOO_LARGE => PanelError::PayloadTooLarge,
            StatusCode::REQUEST_TIMEOUT => PanelError::Timeout,
            StatusCode::UNAUTHORIZED => PanelError::Unauthorized,
            StatusCode::FORBIDDEN => PanelError::Forbidden,
            StatusCode::SERVICE_UNAVAILABLE => PanelError::Busy,
            s if s.is_server_error() => PanelError::Internal,
            _ => PanelError::BadRequest,
        }
    }
}

impl From<crate::db::DbError> for PanelError {
    fn from(e: crate::db::DbError) -> Self {
        PanelError::internal(&e)
    }
}

/// Marker carried in the response extensions until `shape_errors` renders it.
#[derive(Debug, Clone)]
pub struct ErrorMarker(pub PanelError);

/// Set on responses that already are a complete, friendly error (for example
/// the pair page re-rendered with a message); `shape_errors` leaves them alone.
#[derive(Debug, Clone, Copy)]
pub struct Rendered;

impl IntoResponse for PanelError {
    fn into_response(self) -> Response {
        let mut res = Response::new(axum::body::Body::empty());
        *res.status_mut() = self.status();
        if let PanelError::RateLimited { retry_after_secs } = self
            && let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string())
        {
            res.headers_mut().insert(header::RETRY_AFTER, v);
        }
        res.extensions_mut().insert(ErrorMarker(self));
        res
    }
}
