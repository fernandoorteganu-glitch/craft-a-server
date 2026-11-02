//! Logs, the Mods placeholder and Settings (panel address, QR, paired phones).

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};

use super::util::{first_server, render};
use crate::lifecycle::StatusProbe;
use crate::panel::error::PanelError;
use crate::panel::mapping::service_error;
use crate::panel::views::{
    DeviceRow, LOG_LINES, LogsBody, LogsTail, SettingsBody, clean_log_line, mc_address, panel_url,
    qr_svg,
};
use crate::panel::{AuthSession, PanelState, pages, strings as s};
use crate::runtime::ContainerRuntime;
use craft_a_server_core::config::DEFAULT_MC_PORT;

pub async fn logs_page(session: AuthSession) -> Result<Response, PanelError> {
    Ok(pages::screen(s::LOGS_TITLE, "logs", &session, &LogsBody)?.into_response())
}

/// `GET /logs/tail`: the newest lines, polled every two seconds. Each line is
/// cleaned here and HTML-escaped by the template; chat text reaches the log,
/// so a line is never trusted.
pub async fn logs_tail<R, P>(
    State(state): State<PanelState<R, P>>,
    _session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let lines = match first_server(state.service())? {
        Some(row) => state
            .service()
            .logs_tail(&row.id, LOG_LINES)
            .await
            .map_err(|e| service_error(&e))?
            .iter()
            .map(|l| clean_log_line(l))
            .collect(),
        None => Vec::new(),
    };
    Ok(Html(render(&LogsTail { lines })?).into_response())
}

pub async fn settings<R, P>(
    State(state): State<PanelState<R, P>>,
    session: AuthSession,
) -> Result<Response, PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let core = state.core();
    let now = core.now();
    let rows = core.blocking(move |db| Ok(db.list_sessions(now)?)).await?;
    let devices = rows
        .into_iter()
        .map(|r| DeviceRow {
            current: r.id == session.id,
            id: r.id,
            device_name: r.device_name,
            last_seen: pages::format_utc(r.last_seen_unix),
        })
        .collect();

    let cfg = core.config();
    let ip_url = state
        .lan_ip()
        .map(|ip| panel_url(&ip.to_string(), cfg.panel_port));
    // The QR code carries the panel address only, never a pairing code.
    let qr = ip_url.as_deref().and_then(qr_svg);
    let (mc_local, mc_ip) = match first_server(state.service())? {
        Some(row) => (
            Some(mc_address(
                &cfg.panel_hostname,
                row.host_port,
                DEFAULT_MC_PORT,
            )),
            state
                .lan_ip()
                .map(|ip| mc_address(&ip.to_string(), row.host_port, DEFAULT_MC_PORT)),
        ),
        None => (None, None),
    };
    let body = SettingsBody {
        devices,
        csrf_token: &session.csrf_token,
        panel_url: panel_url(&cfg.panel_hostname, cfg.panel_port),
        ip_url,
        qr_svg: qr,
        mc_local,
        mc_ip,
    };
    Ok(pages::screen(s::SETTINGS_TITLE, "settings", &session, &body)?.into_response())
}
