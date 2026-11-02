//! The route table: the single place where screens plug into the panel.
//!
//! Every route is registered with an [`Access`] level. The guard middleware
//! looks the matched path up in that table, so authentication, Origin and CSRF
//! checks are applied centrally; a handler never has to remember them. A route
//! added to the router by other means is not in the table and is treated as
//! [`Access::Session`] (fail closed).

mod assets;
mod backups;
mod mods;
mod online;
mod pair;
mod players;
mod screens;
mod server;
mod sessions;
mod util;

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
        .add("/", Access::Session, get(server::home::<R, P>))
        .add(
            "/partials/server-card",
            Access::Session,
            get(server::card::<R, P>),
        )
        .add(
            "/server/start",
            Access::Session,
            post(server::start::<R, P>),
        )
        .add("/server/stop", Access::Session, post(server::stop::<R, P>))
        .add(
            "/server/restart",
            Access::Session,
            post(server::restart::<R, P>),
        )
        .add(
            "/server/reset",
            Access::Session,
            get(server::reset_confirm::<R, P>).post(server::reset::<R, P>),
        )
        .add(
            "/server/delete",
            Access::Session,
            get(server::delete_confirm::<R, P>).post(server::delete::<R, P>),
        )
        .add(
            "/create",
            Access::Session,
            get(server::create_page_get::<R, P>),
        )
        .add(
            "/server/create",
            Access::Session,
            post(server::create::<R, P>),
        )
        .add("/backups", Access::Session, get(backups::page::<R, P>))
        .add(
            "/partials/backups",
            Access::Session,
            get(backups::list::<R, P>),
        )
        .add(
            "/backups/create",
            Access::Session,
            post(backups::create::<R, P>),
        )
        .add(
            "/backups/{id}/restore",
            Access::Session,
            get(backups::restore_confirm::<R, P>).post(backups::restore::<R, P>),
        )
        .add("/players", Access::Session, get(players::page::<R, P>))
        .add(
            "/players/whitelist/add",
            Access::Session,
            post(players::add::<R, P>),
        )
        .add(
            "/players/whitelist/remove",
            Access::Session,
            post(players::remove::<R, P>),
        )
        .add(
            "/players/kick",
            Access::Session,
            post(players::kick::<R, P>),
        )
        .add("/logs", Access::Session, get(screens::logs_page))
        .add(
            "/logs/tail",
            Access::Session,
            get(screens::logs_tail::<R, P>),
        )
        .add("/mods", Access::Session, get(mods::page::<R, P>))
        .add("/partials/mods", Access::Session, get(mods::list::<R, P>))
        .add("/mods/updates", Access::Session, get(mods::updates::<R, P>))
        .add(
            "/mods/install/{id}",
            Access::Session,
            get(mods::install_confirm::<R, P>).post(mods::install::<R, P>),
        )
        .add(
            "/mods/remove/{id}",
            Access::Session,
            get(mods::remove_confirm::<R, P>).post(mods::remove::<R, P>),
        )
        .add("/online", Access::Session, get(online::page::<R, P>))
        .add(
            "/partials/online",
            Access::Session,
            get(online::status::<R, P>),
        )
        .add(
            "/online/enable",
            Access::Session,
            get(online::enable_confirm::<R, P>).post(online::enable::<R, P>),
        )
        .add(
            "/online/disable",
            Access::Session,
            post(online::disable::<R, P>),
        )
        .add(
            "/settings/devices",
            Access::Session,
            get(screens::settings::<R, P>),
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
