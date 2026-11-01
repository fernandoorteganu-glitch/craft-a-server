//! The route table: the single place where screens plug into the panel.
//!
//! Every route is registered with an [`Access`] level. The guard middleware
//! looks the matched path up in that table, so authentication, Origin and CSRF
//! checks are applied centrally; a handler never has to remember them. A route
//! added to the router by other means is not in the table and is treated as
//! [`Access::Session`] (fail closed).

mod assets;
mod pair;
mod sessions;

use std::collections::HashMap;

use axum::Router;
use axum::routing::{MethodRouter, delete, get, post};

use super::PanelState;

/// Who may call a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Anyone who passed the network checks (health, static files, pair page).
    /// State-changing methods still need a matching `Origin`/`Referer`.
    Public,
    /// A signed-in phone. State-changing methods also need the CSRF token.
    Session,
    /// `POST /api/pair` only: pre-session, so no CSRF token exists yet; it is
    /// protected by the Origin check, the one-time code and the rate limiter.
    Pairing,
}

/// Routes plus their access level, built into a router by [`RouteTable::into_router`].
#[allow(missing_debug_implementations)] // holds handler closures
pub struct RouteTable<S> {
    entries: Vec<(&'static str, Access, MethodRouter<S>)>,
}

impl<S: Clone + Send + Sync + 'static> RouteTable<S> {
    pub fn new() -> Self {
        RouteTable {
            entries: Vec::new(),
        }
    }

    /// Registers `path` (axum syntax, e.g. `/api/things/{id}`) with its methods.
    #[must_use]
    pub fn add(mut self, path: &'static str, access: Access, methods: MethodRouter<S>) -> Self {
        self.entries.push((path, access, methods));
        self
    }

    /// `(path, access)` of every registered route, in registration order.
    pub fn specs(&self) -> Vec<(&'static str, Access)> {
        self.entries.iter().map(|(p, a, _)| (*p, *a)).collect()
    }

    pub(crate) fn access_map(&self) -> HashMap<&'static str, Access> {
        self.specs().into_iter().collect()
    }

    pub(crate) fn into_router(self) -> Router<S> {
        self.entries
            .into_iter()
            .fold(Router::new(), |r, (path, _, methods)| {
                r.route(path, methods)
            })
    }
}

impl<S: Clone + Send + Sync + 'static> Default for RouteTable<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// All routes of the panel. Add new screens here.
pub fn table<R, P>() -> RouteTable<PanelState<R, P>>
where
    R: crate::runtime::ContainerRuntime + 'static,
    P: crate::lifecycle::StatusProbe + 'static,
{
    RouteTable::new()
        .add("/healthz", Access::Public, get(assets::healthz))
        .add("/static/{*file}", Access::Public, get(assets::serve_static))
        .add("/pair", Access::Public, get(pair::page))
        .add("/api/pair", Access::Pairing, post(pair::submit))
        .add("/", Access::Session, get(sessions::home))
        .add(
            "/settings/devices",
            Access::Session,
            get(sessions::devices_page),
        )
        .add("/api/logout", Access::Session, post(sessions::logout))
        .add("/api/sessions", Access::Session, get(sessions::list))
        .add(
            "/api/sessions/{id}",
            Access::Session,
            delete(sessions::revoke),
        )
        .add(
            "/api/sessions/{id}/revoke",
            Access::Session,
            post(sessions::revoke_form),
        )
}
