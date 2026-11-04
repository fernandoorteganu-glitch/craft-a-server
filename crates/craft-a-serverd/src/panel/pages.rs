//! Templates and the base layout.
//!
//! Every screen is rendered in two steps: the screen's own template produces
//! the page body, then [`page`] wraps it in `templates/layout.html` (head,
//! navigation, CSRF header for htmx, footer). Templates are compiled into the
//! binary and auto-escape their variables; they must not contain inline
//! scripts, styles or event handlers (the CSP blocks them).

use askama::Template;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

use super::auth::AuthSession;
use super::error::PanelError;

#[derive(Template)]
#[template(path = "layout.html")]
struct Layout<'a> {
    title: &'a str,
    /// `Some` only for signed-in pages; becomes the htmx `X-CSRF-Token` header.
    csrf_token: Option<&'a str>,
    signed_in: bool,
    /// Which navigation entry to mark as the current page.
    active: &'a str,
    content: &'a str,
    /// Fingerprint of the stylesheet, for its cache-busting address.
    css_version: &'static str,
}

/// Wraps a rendered body template in the base layout.
///
/// Pass the request's [`AuthSession`] for signed-in screens (it enables the
/// navigation bar and the htmx CSRF header) or `None` for public pages.
pub fn page<T: Template>(
    title: &str,
    session: Option<&AuthSession>,
    body: &T,
) -> Result<Html<String>, PanelError> {
    render(title, "", session, body)
}

/// A signed-in screen with its navigation entry highlighted
/// (`home`, `backups`, `players`, `logs`, `mods`, `performance`, `online` or `settings`).
pub fn screen<T: Template>(
    title: &str,
    active: &str,
    session: &AuthSession,
    body: &T,
) -> Result<Html<String>, PanelError> {
    render(title, active, Some(session), body)
}

fn render<T: Template>(
    title: &str,
    active: &str,
    session: Option<&AuthSession>,
    body: &T,
) -> Result<Html<String>, PanelError> {
    let content = body.render().map_err(|e| PanelError::internal(&e))?;
    let layout = Layout {
        title,
        csrf_token: session.map(|s| s.csrf_token.as_str()),
        signed_in: session.is_some(),
        active,
        content: &content,
        css_version: crate::panel::routes::assets::css_version(),
    };
    layout
        .render()
        .map(Html)
        .map_err(|e| PanelError::internal(&e))
}

/// Body of the pair page.
#[derive(Template, Debug)]
#[template(path = "pair.html")]
pub struct PairBody<'a> {
    pub error: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorBody<'a> {
    message: &'a str,
    status: u16,
}

/// The pair page with an optional friendly error, as a complete response.
pub fn pair_page(error: Option<&str>) -> Result<Html<String>, PanelError> {
    page(super::strings::PAIR_TITLE, None, &PairBody { error })
}

/// The response for an error: JSON for API callers, a page otherwise.
pub fn error_response(err: &PanelError, json: bool) -> Response {
    let status = err.status();
    let message = err.message();
    let mut res = if json {
        let body = serde_json::json!({ "error": { "code": err.code(), "message": message } });
        let mut r = Response::new(axum::body::Body::from(body.to_string()));
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        r
    } else {
        let rendered = page(
            super::strings::ERR_PAGE_TITLE,
            None,
            &ErrorBody {
                message: &message,
                status: status.as_u16(),
            },
        );
        match rendered {
            Ok(html) => html.into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                super::strings::ERR_INTERNAL,
            )
                .into_response(),
        }
    };
    *res.status_mut() = status;
    res
}

/// `2026-11-01 12:00 UTC` (UTC, minute precision) for display.
pub fn format_utc(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        secs / 3600,
        secs % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00 UTC");
    }
}
