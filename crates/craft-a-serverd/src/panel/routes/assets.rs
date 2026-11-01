//! `/healthz` and `/static/*`, served from memory.

use axum::extract::Path;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

use crate::panel::error::PanelError;

const HTMX: &[u8] = include_bytes!("../../../static/htmx.min.js");
const PANEL_CSS: &[u8] = include_bytes!("../../../static/panel.css");

/// `(file name, content type, bytes)` of every static asset.
pub const ASSETS: &[(&str, &str, &[u8])] = &[
    ("htmx.min.js", "text/javascript; charset=utf-8", HTMX),
    ("panel.css", "text/css; charset=utf-8", PANEL_CSS),
];

pub async fn healthz() -> &'static str {
    "ok"
}

pub async fn serve_static(Path(file): Path<String>) -> Result<Response, PanelError> {
    let (_, content_type, bytes) = ASSETS
        .iter()
        .find(|(name, _, _)| *name == file)
        .ok_or(PanelError::NotFound)?;
    let mut res = (*bytes).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    Ok(res)
}
