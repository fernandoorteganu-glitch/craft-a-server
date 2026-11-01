//! Small helpers shared by the screen handlers.

use askama::Template;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::db::ServerRow;
use crate::lifecycle::{Service, StatusProbe};
use crate::panel::error::PanelError;
use crate::panel::mapping::service_error;
use crate::panel::strings as s;
use crate::runtime::ContainerRuntime;

/// `303 See Other` back to a page after a form post.
pub fn redirect(to: &'static str) -> Response {
    Redirect::to(to).into_response()
}

/// The one server of the MVP, if it exists.
pub fn first_server<R: ContainerRuntime, P: StatusProbe>(
    service: &Service<R, P>,
) -> Result<Option<ServerRow>, PanelError> {
    Ok(service
        .list_servers()
        .map_err(|e| service_error(&e))?
        .into_iter()
        .next())
}

/// The one server, or a friendly "create one first".
pub fn require_server<R: ContainerRuntime, P: StatusProbe>(
    service: &Service<R, P>,
) -> Result<ServerRow, PanelError> {
    first_server(service)?.ok_or_else(|| PanelError::Conflict(s::ERR_NO_SERVER.to_owned()))
}

/// Renders a partial template to a string.
pub fn render<T: Template>(template: &T) -> Result<String, PanelError> {
    template.render().map_err(|e| PanelError::internal(&e))
}

/// The hidden `confirm=yes` field of the yes/no pages.
#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    confirm: Option<String>,
}

impl ConfirmForm {
    /// Destructive actions run only when the confirm step was really passed.
    pub fn require(&self) -> Result<(), PanelError> {
        if self.confirm.as_deref() == Some("yes") {
            Ok(())
        } else {
            Err(PanelError::Invalid(s::ERR_CONFIRM.to_owned()))
        }
    }
}
