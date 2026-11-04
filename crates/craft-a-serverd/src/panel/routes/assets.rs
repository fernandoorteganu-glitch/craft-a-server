//! `/healthz` and `/static/*`, served from memory.

use axum::extract::Path;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};

use crate::panel::error::PanelError;

const HTMX: &[u8] = include_bytes!("../../../static/htmx.min.js");
const PANEL_CSS: &[u8] = include_bytes!("../../../static/panel.css");
const WORLD_JS: &[u8] = include_bytes!("../../../static/world.js");

/// `(file name, content type, bytes)` of every static asset.
pub const ASSETS: &[(&str, &str, &[u8])] = &[
    ("htmx.min.js", "text/javascript; charset=utf-8", HTMX),
    ("panel.css", "text/css; charset=utf-8", PANEL_CSS),
    ("world.js", "text/javascript; charset=utf-8", WORLD_JS),
];

/// A short fingerprint of the stylesheet, put in its address so a browser
/// that cached an older one fetches the new one as soon as it changes.
pub fn css_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(PANEL_CSS);
        digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
    })
}

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
