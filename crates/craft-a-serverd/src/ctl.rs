//! Control socket: serves the CLI protocol from `craft_a_server_core::ctl`.
//!
//! One task per connection. Requests are read as bounded lines, dispatched to
//! the lifecycle [`Service`], and answered with typed replies. Only the daemon's
//! own user (and root) may connect; the check uses `SO_PEERCRED`, so file modes
//! are defence in depth, not the only gate.
#![cfg(unix)]

use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use craft_a_server_core::ctl::{
    BackupInfo, Command, ConsoleAction, DeviceInfo, DiagnoseInfo, DiskInfo, ErrorKind, EventInfo,
    GameUseInfo, HostUseInfo, MAX_REQUEST_BYTES, PROTOCOL_VERSION, ProtocolError, Reply, Request,
    ResourcesInfo, Response, ServerInfo, ViewDistanceInfo, encode_line, parse_request,
};
use craft_a_server_core::settings::{VIEW_DISTANCE_DEFAULT, VIEW_DISTANCE_MAX, VIEW_DISTANCE_MIN};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::db::{BackupRow, ServerRow};
use crate::lifecycle::{Console, Reason, Resources, Service, ServiceError, StatusProbe};
use crate::runtime::{ContainerRuntime, RuntimeError};
use crate::tunnel::TunnelError;

const MAX_CONNECTIONS: usize = 64;
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_GRACE: Duration = Duration::from_secs(20);
const MAX_LOG_LINES: u32 = 2000;
const DIAGNOSE_EVENTS: u32 = 20;

/// Binds the socket and serves until `shutdown` resolves.
///
/// On shutdown it stops accepting, gives running requests a short grace period,
/// and removes the socket file. Game servers are not touched.
pub async fn serve<R, P, F>(
    service: Arc<Service<R, P>>,
    socket_path: &Path,
    shutdown: F,
) -> io::Result<()>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
    F: Future<Output = ()> + Send,
{
    let listener = bind(socket_path).await?;
    tracing::info!(socket = %socket_path.display(), "control socket listening");
    let own_uid = rustix::process::getuid().as_raw();
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut tasks: JoinSet<()> = JoinSet::new();

    tokio::pin!(shutdown);
    loop {
        // Reap finished connection tasks so the set does not grow.
        while tasks.try_join_next().is_some() {}
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    tracing::warn!("too many control connections, dropping one");
                    continue;
                };
                let svc = Arc::clone(&service);
                let stop = stop_rx.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    handle_connection(svc, stream, own_uid, stop).await;
                });
            }
        }
    }

    tracing::info!("control socket shutting down");
    drop(listener);
    let _ = stop_tx.send(true);
    let drained = tokio::time::timeout(DRAIN_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!("aborting control requests still running after the grace period");
        tasks.abort_all();
    }
    if let Err(e) = std::fs::remove_file(socket_path)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(error = %e, "could not remove the control socket");
    }
    Ok(())
}

/// Creates the parent directory (0750), clears a stale socket and binds (0660).
async fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(parent)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            // Only a socket nobody answers on is stale.
            if let Ok(Ok(_)) =
                tokio::time::timeout(STALE_PROBE_TIMEOUT, UnixStream::connect(path)).await
            {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another daemon is already listening on {}", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

/// Same user as the daemon, or root.
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid || peer_uid == 0
}

async fn handle_connection<R, P>(
    svc: Arc<Service<R, P>>,
    stream: UnixStream,
    own_uid: u32,
    mut stop: watch::Receiver<bool>,
) where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    match rustix::net::sockopt::socket_peercred(&stream) {
        Ok(cred) if peer_allowed(cred.uid.as_raw(), own_uid) => {}
        Ok(cred) => {
            tracing::warn!(uid = cred.uid.as_raw(), "rejected control connection");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot read peer credentials, rejecting connection");
            return;
        }
    }

    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    loop {
        let read = tokio::select! {
            r = tokio::time::timeout(IDLE_TIMEOUT, read_line_bounded(&mut reader, MAX_REQUEST_BYTES)) => r,
            () = stopped(&mut stop) => return,
        };
        let line = match read {
            Err(_elapsed) => return,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "control read failed");
                return;
            }
            Ok(Ok(LineRead::Eof)) => return,
            Ok(Ok(LineRead::TooLong)) => {
                let _ = send(
                    &mut writer,
                    None,
                    error(ErrorKind::InvalidInput, ProtocolError::TooLarge.to_string()),
                )
                .await;
                return;
            }
            Ok(Ok(LineRead::Line(bytes))) => bytes,
        };
        let request = match std::str::from_utf8(&line) {
            Ok(text) => parse_request(text),
            Err(_) => Err(ProtocolError::Malformed(
                "request is not valid UTF-8".into(),
            )),
        };
        let Request { id, cmd } = match request {
            Ok(r) => r,
            Err(e) => {
                if send(
                    &mut writer,
                    None,
                    error(ErrorKind::InvalidInput, e.to_string()),
                )
                .await
                .is_err()
                {
                    return;
                }
                continue;
            }
        };
        if let Command::LogsFollow { id: server } = &cmd {
            follow_logs(&svc, server, id, &mut reader, &mut writer, &mut stop).await;
            return;
        }
        let response = match run(&svc, cmd).await {
            Ok(r) => r,
            Err(e) => error_response(&e),
        };
        if send(&mut writer, id, response).await.is_err() {
            return;
        }
    }
}

/// Resolves once shutdown was requested. Returns `()` so no watch guard is
/// held across an await in callers (the guard is not `Send`).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|v| *v).await;
}

enum LineRead {
    Line(Vec<u8>),
    Eof,
    TooLong,
}

/// Reads up to a newline without ever buffering more than `max` bytes.
async fn read_line_bounded<T: AsyncBufRead + Unpin>(
    reader: &mut T,
    max: usize,
) -> io::Result<LineRead> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if line.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line(line)
            });
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let take = newline.map_or(chunk.len(), |i| i + 1);
        if line.len() + take > max {
            return Ok(LineRead::TooLong);
        }
        line.extend_from_slice(chunk.get(..take).unwrap_or_default());
        Pin::new(&mut *reader).consume(take);
        if newline.is_some() {
            return Ok(LineRead::Line(line));
        }
    }
}

async fn send(writer: &mut OwnedWriteHalf, id: Option<u64>, response: Response) -> io::Result<()> {
    let line = encode_line(&Reply { id, response });
    let write = async {
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await
    };
    tokio::time::timeout(WRITE_TIMEOUT, write)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client too slow"))?
}

/// Streams log lines until the client disconnects, the log ends or we shut down.
async fn follow_logs<R, P>(
    svc: &Service<R, P>,
    server: &str,
    id: Option<u64>,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
    stop: &mut watch::Receiver<bool>,
) where
    R: ContainerRuntime,
    P: StatusProbe,
{
    let mut rx = match svc.get_server(server) {
        Ok(_) => match svc.rt.logs_follow(server).await {
            Ok(rx) => rx,
            Err(e) => {
                let _ = send(writer, id, error_response(&ServiceError::Runtime(e))).await;
                return;
            }
        },
        Err(e) => {
            let _ = send(writer, id, error_response(&e)).await;
            return;
        }
    };
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(text) => {
                    // The bounded write is the backpressure: a slow client
                    // stalls this task, never the daemon.
                    if send(writer, id, Response::Line(text)).await.is_err() {
                        return;
                    }
                }
                None => {
                    let _ = send(writer, id, Response::Ok).await;
                    return;
                }
            },
            // The client has nothing more to say; any read result other than
            // fresh data means it went away.
            incoming = reader.fill_buf() => match incoming {
                Ok(buf) if !buf.is_empty() => {
                    let n = buf.len();
                    Pin::new(&mut *reader).consume(n);
                }
                _ => return,
            },
            () = stopped(stop) => return,
        }
    }
}

fn error(kind: ErrorKind, message: impl Into<String>) -> Response {
    Response::Error {
        kind,
        message: message.into(),
    }
}

/// Maps a service failure to a stable kind and a message that is safe to show.
///
/// Internal failures (database, container engine, i/o, backup archive) are
/// logged in full and reported generically, so paths and SQL never reach a client.
pub fn error_response(e: &ServiceError) -> Response {
    match e {
        ServiceError::InvalidInput(_) => error(ErrorKind::InvalidInput, e.to_string()),
        ServiceError::NotFound(_) => error(ErrorKind::NotFound, e.to_string()),
        ServiceError::WrongState { .. } => error(ErrorKind::WrongState, e.to_string()),
        ServiceError::EulaNotAccepted => error(ErrorKind::Eula, e.to_string()),
        ServiceError::StartFailed(_) => error(ErrorKind::StartFailed, e.to_string()),
        ServiceError::DiskLow => error(ErrorKind::DiskLow, e.to_string()),
        ServiceError::Memory(_) => error(ErrorKind::Memory, e.to_string()),
        ServiceError::Mods(m) => error(
            ErrorKind::Mods,
            crate::mods::logic::clean_text(&m.to_string(), 600),
        ),
        ServiceError::Tunnel(TunnelError::Db(_) | TunnelError::Io(_)) => {
            tracing::error!(error = %e, "internal error changing online play");
            error(
                ErrorKind::Internal,
                "internal error; the details are in the daemon log (journalctl -u craft-a-serverd)",
            )
        }
        ServiceError::Tunnel(t) => error(ErrorKind::Tunnel, t.to_string()),
        ServiceError::WorldImport(m) => error(ErrorKind::InvalidInput, m.clone()),
        ServiceError::Runtime(RuntimeError::NotFound(_)) => error(
            ErrorKind::WrongState,
            "the server is not running".to_owned(),
        ),
        ServiceError::Runtime(RuntimeError::InvalidCommand(m)) => {
            error(ErrorKind::InvalidInput, format!("invalid command: {m}"))
        }
        ServiceError::Db(_)
        | ServiceError::Runtime(_)
        | ServiceError::Backup(_)
        | ServiceError::Io { .. } => {
            tracing::error!(error = %e, debug = ?e, "internal error serving a control request");
            error(
                ErrorKind::Internal,
                "internal error; the details are in the daemon log (journalctl -u craft-a-serverd)",
            )
        }
    }
}

fn server_info(row: &ServerRow) -> ServerInfo {
    let (state, reason) = row.state.to_parts();
    ServerInfo {
        id: row.id.clone(),
        name: row.name.clone(),
        mc_version: row.mc_version.clone(),
        state: state.to_owned(),
        reason: reason.map(str::to_owned),
        port: row.host_port,
        created_unix: row.created_unix,
    }
}

/// Thousandths of a core, rounded; a nonsense reading becomes zero.
fn millicores(cores: f64) -> u32 {
    if cores.is_finite() && cores > 0.0 {
        (cores * 1000.0).round().min(f64::from(u32::MAX)) as u32
    } else {
        0
    }
}

fn resources_info(r: &Resources) -> ResourcesInfo {
    ResourcesInfo {
        running: r.running,
        game: r.game.map(|g| GameUseInfo {
            mem_used_bytes: g.mem_used_bytes,
            cpu_millicores: g.cpu_cores.map(millicores),
            pids: g.pids,
        }),
        memory_limit_bytes: r.memory_limit_bytes,
        heap_bytes: r.heap_bytes,
        cpus: r.cpus,
        pids_limit: r.pids_limit,
        host: r.host.map(|h| HostUseInfo {
            mem_total_bytes: h.mem_total_bytes,
            mem_available_bytes: h.mem_available_bytes,
            load1_centis: millicores(h.load1) / 10,
        }),
        disk: r.disk.map(|d| DiskInfo {
            free_bytes: d.free_bytes,
            total_bytes: d.total_bytes,
        }),
    }
}

fn view_distance_info(value: Option<u8>) -> ViewDistanceInfo {
    ViewDistanceInfo {
        value,
        default_value: VIEW_DISTANCE_DEFAULT,
        min: VIEW_DISTANCE_MIN,
        max: VIEW_DISTANCE_MAX,
    }
}

fn backup_info(row: &BackupRow) -> BackupInfo {
    BackupInfo {
        id: row.id.clone(),
        server_id: row.server_id.clone(),
        kind: row.kind.clone(),
        size_bytes: row.size_bytes,
        created_unix: row.created_unix,
        verified: row.verified,
    }
}

/// Executes one non-streaming command.
async fn run<R, P>(svc: &Service<R, P>, cmd: Command) -> Result<Response, ServiceError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    Ok(match cmd {
        Command::Ping {} => Response::Pong {
            protocol: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        Command::List {} => {
            Response::Servers(svc.list_servers()?.iter().map(server_info).collect())
        }
        Command::Create {
            name,
            mc_version,
            eula_accepted,
        } => {
            let row = svc.create_server(&name, &mc_version, eula_accepted).await?;
            Response::Server(server_info(&row))
        }
        Command::Start { id } => {
            svc.start(&id).await?;
            Response::Server(server_info(&svc.get_server(&id)?))
        }
        Command::Stop { id } => {
            svc.stop(&id).await?;
            Response::Server(server_info(&svc.get_server(&id)?))
        }
        Command::Restart { id } => {
            svc.restart(&id).await?;
            Response::Server(server_info(&svc.get_server(&id)?))
        }
        Command::Reset { id } => {
            svc.reset(&id).await?;
            Response::Server(server_info(&svc.get_server(&id)?))
        }
        Command::Delete { id } => {
            svc.delete_server(&id).await?;
            Response::Ok
        }
        Command::Status { id } => Response::Server(server_info(&svc.get_server(&id)?)),
        Command::BackupCreate { id } => {
            Response::Backup(backup_info(&svc.backup(&id, Reason::Manual).await?))
        }
        Command::BackupList { id } => {
            Response::Backups(svc.list_backups(&id)?.iter().map(backup_info).collect())
        }
        Command::BackupRestore { id, backup_id } => {
            svc.restore(&id, &backup_id).await?;
            Response::Ok
        }
        Command::Console { id, action, player } => {
            let console = console_command(action, player)?;
            Response::Text(svc.console(&id, &console).await?)
        }
        Command::LogsTail { id, lines } => {
            svc.get_server(&id)?;
            let lines = lines.clamp(1, MAX_LOG_LINES) as usize;
            Response::Text(svc.rt.logs_tail(&id, lines).await?.join("\n"))
        }
        // Handled by the connection loop; reaching here would be a bug, not a crash.
        Command::LogsFollow { .. } => {
            error(ErrorKind::InvalidInput, "logs_follow cannot be used here")
        }
        Command::Diagnose {} => Response::Diagnose(diagnose(svc).await?),
        Command::Pair {} => {
            let issued = crate::panel::auth::issue_pairing_code(
                svc.db(),
                svc.now(),
                svc.cfg.pairing_code_ttl_secs,
            )
            .map_err(|e| ServiceError::Io {
                context: "creating a pairing code",
                source: io::Error::other(e.to_string()),
            })?;
            let (url, alt_urls) = crate::panel::pairing_urls(&svc.cfg, crate::panel::lan_ipv4());
            svc.event(
                None,
                "pairing_code",
                "a pairing code was created from the CLI",
            );
            Response::PairingCode {
                code: issued.code,
                expires_unix: issued.expires_unix,
                url,
                alt_urls,
            }
        }
        Command::DevicesList {} => Response::Devices(
            svc.db()
                .list_sessions(svc.now())?
                .into_iter()
                .map(|s| DeviceInfo {
                    id: s.id,
                    device_name: s.device_name,
                    created_unix: s.created_unix,
                    last_seen_unix: s.last_seen_unix,
                    expires_unix: s.expires_unix,
                })
                .collect(),
        ),
        Command::ModsSearch {
            id,
            query,
            include_unknown,
            offset,
        } => Response::ModHits(
            svc.mods_search(&id, &query, include_unknown, offset)
                .await?,
        ),
        Command::ModsList { id } => Response::ModList(svc.mods_list(&id).await?),
        Command::ModsPlan {
            id,
            project,
            allow_prerelease,
        } => Response::ModPlan(svc.mods_plan(&id, &project, allow_prerelease).await?),
        Command::ModsInstall {
            id,
            project,
            allow_prerelease,
        } => Response::ModChange(svc.mods_install(&id, &project, allow_prerelease).await?),
        Command::ModsRemovePlan { id, project } => {
            Response::ModRemovePlan(svc.mods_remove_plan(&id, &project)?)
        }
        Command::ModsRemove {
            id,
            project,
            force,
            remove_orphans,
        } => Response::ModChange(
            svc.mods_remove(&id, &project, force, remove_orphans)
                .await?,
        ),
        Command::ModsCheck { id } => Response::ModUpdates(svc.mods_check(&id).await?),
        Command::Resources { id } => {
            Response::Resources(resources_info(&svc.resources(&id).await?))
        }
        Command::ViewDistanceGet { id } => {
            Response::ViewDistance(view_distance_info(svc.view_distance(&id)?))
        }
        Command::ViewDistanceSet { id, value } => {
            svc.set_view_distance(&id, value)?;
            Response::ViewDistance(view_distance_info(svc.view_distance(&id)?))
        }
        Command::TunnelStatus {} => Response::Tunnel(svc.tunnel_info()?),
        Command::TunnelEnable { confirm } => Response::Tunnel(svc.tunnel_enable(confirm)?),
        Command::TunnelDisable { forget_claim } => {
            Response::Tunnel(svc.tunnel_disable(forget_claim).await?)
        }
        Command::DevicesRevoke { id } => {
            if svc.db().delete_session(&id)? {
                svc.event(
                    None,
                    "device_revoked",
                    &format!("device {id} was signed out"),
                );
                Response::Ok
            } else {
                return Err(ServiceError::NotFound(format!("device {id}")));
            }
        }
    })
}

fn console_command(action: ConsoleAction, player: Option<String>) -> Result<Console, ServiceError> {
    let need = |player: Option<String>| {
        player.ok_or_else(|| ServiceError::InvalidInput("this action needs a player name".into()))
    };
    Ok(match action {
        ConsoleAction::WhitelistAdd => Console::WhitelistAdd(need(player)?),
        ConsoleAction::WhitelistRemove => Console::WhitelistRemove(need(player)?),
        ConsoleAction::Kick => Console::Kick(need(player)?),
        ConsoleAction::WhitelistList => Console::WhitelistList,
    })
}

async fn diagnose<R, P>(svc: &Service<R, P>) -> Result<DiagnoseInfo, ServiceError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
{
    let podman_version = match svc.rt.version().await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "diagnose: podman version unavailable");
            None
        }
    };
    // Events are free text written by the daemon itself; server rows are
    // converted through `server_info`, which has no field for the RCON password.
    let events = svc
        .db()
        .recent_events(DIAGNOSE_EVENTS)?
        .into_iter()
        .map(|e| EventInfo {
            ts_unix: e.ts_unix,
            server_id: e.server_id,
            kind: e.kind,
            detail: e.detail,
        })
        .collect();
    Ok(DiagnoseInfo {
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: PROTOCOL_VERSION,
        podman_version,
        schema_version: u32::try_from(svc.db().schema_version()?).unwrap_or(u32::MAX),
        servers: svc.list_servers()?.iter().map(server_info).collect(),
        events,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use craft_a_server_core::config::Config;
    use craft_a_server_core::ctl::parse_reply;
    use craft_a_server_core::mc_protocol::ServerStatus;
    use tokio::io::AsyncReadExt;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::db::Db;
    use crate::lifecycle::{DiskUsage, Env};
    use crate::runtime::FakeRuntime;

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    const GIB: u64 = 1024 * 1024 * 1024;

    struct UpProbe(AtomicUsize);

    impl StatusProbe for UpProbe {
        async fn status(&self, _port: u16) -> Option<ServerStatus> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Some(ServerStatus {
                version_name: "26.3".into(),
                protocol: 777,
                players_online: 0,
                players_max: 20,
            })
        }
    }

    type Svc = Service<FakeRuntime, UpProbe>;

    struct Fixture {
        dir: tempfile::TempDir,
        svc: Arc<Svc>,
        rt: FakeRuntime,
        socket: PathBuf,
        stop: Option<oneshot::Sender<()>>,
        task: Option<JoinHandle<io::Result<()>>>,
    }

    impl Fixture {
        fn new() -> Result<Fixture, Box<dyn std::error::Error>> {
            let dir = tempfile::tempdir()?;
            let cfg = Config {
                data_dir: dir.path().join("data"),
                start_timeout_secs: 1,
                stop_timeout_secs: 5,
                ..Config::default()
            };
            let rt = FakeRuntime::new();
            let env = Env {
                total_ram_bytes: 8 * GIB,
                uid: 1000,
                gid: 1000,
                now_unix: Arc::new(|| 1_700_000_000),
                disk: Arc::new(|_| {
                    Ok(DiskUsage {
                        free_bytes: 90,
                        total_bytes: 100,
                    })
                }),
                poll_interval: Duration::from_millis(10),
            };
            let db = Db::open_in_memory()?;
            let tunnel = Arc::new(crate::tunnel::TunnelManager::new(
                crate::tunnel::TunnelSettings {
                    dirs: crate::tunnel::TunnelDirs::new(&cfg.tunnel_dir()),
                    mc_port: cfg.mc_port,
                    release: crate::tunnel::Release::pinned(),
                    platform: Some(crate::tunnel::Platform::LinuxAmd64),
                    rundata_url: "https://api.playit.invalid/agents/rundata".to_owned(),
                    timings: crate::tunnel::Timings::default(),
                },
                db.clone(),
                Arc::new(crate::tunnel::FakeFetch::default()),
                Arc::clone(&env.now_unix),
            ));
            let svc = Arc::new(
                Service::new(
                    Arc::new(cfg),
                    db,
                    Arc::new(rt.clone()),
                    Arc::new(UpProbe(AtomicUsize::new(0))),
                    env,
                )
                .with_tunnel(tunnel),
            );
            let socket = dir.path().join("run").join("ctl.sock");
            Ok(Fixture {
                dir,
                svc,
                rt,
                socket,
                stop: None,
                task: None,
            })
        }

        async fn serve(&mut self) -> TestResult {
            let (tx, rx) = oneshot::channel::<()>();
            let svc = Arc::clone(&self.svc);
            let socket = self.socket.clone();
            self.task = Some(tokio::spawn(async move {
                serve(svc, &socket, async {
                    let _ = rx.await;
                })
                .await
            }));
            self.stop = Some(tx);
            for _ in 0..200 {
                if UnixStream::connect(&self.socket).await.is_ok() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err("socket never came up".into())
        }

        async fn shutdown(&mut self) -> TestResult {
            if let Some(tx) = self.stop.take() {
                let _ = tx.send(());
            }
            if let Some(task) = self.task.take() {
                task.await??;
            }
            Ok(())
        }

        async fn client(&self) -> io::Result<Client> {
            let stream = UnixStream::connect(&self.socket).await?;
            let (r, w) = stream.into_split();
            Ok(Client {
                reader: BufReader::new(r),
                writer: w,
            })
        }
    }

    struct Client {
        reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        writer: OwnedWriteHalf,
    }

    impl Client {
        async fn raw(&mut self, text: &str) -> io::Result<()> {
            self.writer.write_all(text.as_bytes()).await
        }

        async fn read_reply(&mut self) -> Result<Reply, Box<dyn std::error::Error>> {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
                .await??;
            if n == 0 {
                return Err("connection closed".into());
            }
            Ok(parse_reply(&line)?)
        }

        async fn call(
            &mut self,
            id: u64,
            cmd: Command,
        ) -> Result<Response, Box<dyn std::error::Error>> {
            self.raw(&encode_line(&Request { id: Some(id), cmd }))
                .await?;
            let reply = self.read_reply().await?;
            assert_eq!(reply.id, Some(id));
            Ok(reply.response)
        }
    }

    fn kind_of(r: &Response) -> Option<ErrorKind> {
        match r {
            Response::Error { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    #[tokio::test]
    async fn full_flow_over_a_real_socket() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;

        assert!(matches!(
            c.call(1, Command::Ping {}).await?,
            Response::Pong {
                protocol: PROTOCOL_VERSION,
                ..
            }
        ));
        let Response::Server(created) = c
            .call(
                2,
                Command::Create {
                    name: "Survival".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: true,
                },
            )
            .await?
        else {
            return Err("expected a server".into());
        };
        assert_eq!(created.state, "stopped");
        let id = created.id;

        let Response::Server(started) = c.call(3, Command::Start { id: id.clone() }).await? else {
            return Err("expected a server".into());
        };
        assert_eq!(started.state, "running");

        let Response::Servers(list) = c.call(4, Command::List {}).await? else {
            return Err("expected servers".into());
        };
        assert_eq!(list.len(), 1);

        let text = c
            .call(
                5,
                Command::Console {
                    id: id.clone(),
                    action: ConsoleAction::WhitelistAdd,
                    player: Some("Steve".into()),
                },
            )
            .await?;
        assert!(matches!(text, Response::Text(ref t) if t.contains("whitelist add Steve")));

        // A world must exist for the backup to verify.
        let world = fx
            .dir
            .path()
            .join("data/servers")
            .join(&id)
            .join("data/world");
        std::fs::create_dir_all(&world)?;
        std::fs::write(world.join("level.dat"), b"x")?;
        let Response::Backup(b) = c.call(6, Command::BackupCreate { id: id.clone() }).await? else {
            return Err("expected a backup".into());
        };
        let Response::Backups(bs) = c.call(7, Command::BackupList { id: id.clone() }).await? else {
            return Err("expected backups".into());
        };
        assert_eq!(bs.len(), 1);
        assert_eq!(bs.first().map(|x| x.id.clone()), Some(b.id));

        fx.rt.push_log(&id, "hello");
        let Response::Text(logs) = c
            .call(
                8,
                Command::LogsTail {
                    id: id.clone(),
                    lines: 10,
                },
            )
            .await?
        else {
            return Err("expected text".into());
        };
        assert!(logs.contains("hello"));

        let Response::Server(stopped) = c.call(9, Command::Stop { id: id.clone() }).await? else {
            return Err("expected a server".into());
        };
        assert_eq!(stopped.state, "stopped");
        assert_eq!(c.call(10, Command::Delete { id }).await?, Response::Ok);
        fx.shutdown().await
    }

    #[tokio::test]
    async fn errors_have_stable_kinds() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;

        let r = c.call(1, Command::Status { id: "nope".into() }).await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::NotFound));
        let r = c
            .call(
                2,
                Command::Create {
                    name: "x".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: false,
                },
            )
            .await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::Eula));
        let r = c
            .call(
                3,
                Command::Create {
                    name: "".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: true,
                },
            )
            .await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::InvalidInput));

        let Response::Server(s) = c
            .call(
                4,
                Command::Create {
                    name: "a".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: true,
                },
            )
            .await?
        else {
            return Err("expected a server".into());
        };
        // Stopped server: console needs a running one.
        let r = c
            .call(
                5,
                Command::Console {
                    id: s.id.clone(),
                    action: ConsoleAction::WhitelistList,
                    player: None,
                },
            )
            .await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::WrongState));
        // Missing player.
        c.call(6, Command::Start { id: s.id.clone() }).await?;
        let r = c
            .call(
                7,
                Command::Console {
                    id: s.id.clone(),
                    action: ConsoleAction::Kick,
                    player: None,
                },
            )
            .await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::InvalidInput));
        let r = c
            .call(
                8,
                Command::Console {
                    id: s.id.clone(),
                    action: ConsoleAction::Kick,
                    player: Some("bad name; stop".into()),
                },
            )
            .await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::InvalidInput));
        let r = c.call(9, Command::Delete { id: s.id }).await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::WrongState));
        fx.shutdown().await
    }

    #[tokio::test]
    async fn bad_requests_get_an_error_and_the_connection_survives() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;

        c.raw("not json\n").await?;
        let r = c.read_reply().await?;
        assert_eq!(kind_of(&r.response), Some(ErrorKind::InvalidInput));
        c.raw("{\"cmd\":{\"type\":\"format_disk\"}}\n").await?;
        let r = c.read_reply().await?;
        assert!(matches!(
            r.response,
            Response::Error { kind: ErrorKind::InvalidInput, ref message } if message.contains("format_disk")
        ));
        assert!(matches!(
            c.call(1, Command::Ping {}).await?,
            Response::Pong { .. }
        ));
        fx.shutdown().await
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_and_closes() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;
        let big = "a".repeat(MAX_REQUEST_BYTES + 10);
        c.raw(&big).await?;
        let r = c.read_reply().await?;
        assert_eq!(kind_of(&r.response), Some(ErrorKind::InvalidInput));
        let mut rest = Vec::new();
        let n =
            tokio::time::timeout(Duration::from_secs(5), c.reader.read_to_end(&mut rest)).await??;
        assert_eq!(n, 0);
        fx.shutdown().await
    }

    #[tokio::test]
    async fn socket_modes_and_cleanup() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let sock_mode = std::fs::metadata(&fx.socket)?.permissions().mode() & 0o777;
        assert_eq!(sock_mode, 0o660);
        let parent = fx.socket.parent().ok_or("no parent")?;
        assert_eq!(
            std::fs::metadata(parent)?.permissions().mode() & 0o777,
            0o750
        );
        fx.shutdown().await?;
        assert!(!fx.socket.exists());
        Ok(())
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_but_live_one_is_not() -> TestResult {
        let mut fx = Fixture::new()?;
        let parent = fx.socket.parent().ok_or("no parent")?.to_path_buf();
        std::fs::create_dir_all(&parent)?;
        // A socket file nobody listens on.
        drop(std::os::unix::net::UnixListener::bind(&fx.socket)?);
        assert!(fx.socket.exists());
        fx.serve().await?;

        // A second daemon must refuse to steal the live socket.
        let (_tx, rx) = oneshot::channel::<()>();
        let second = serve(Arc::clone(&fx.svc), &fx.socket, async {
            let _ = rx.await;
        })
        .await;
        assert!(matches!(second, Err(ref e) if e.kind() == io::ErrorKind::AddrInUse));
        // ...and the first one still answers.
        let mut c = fx.client().await?;
        assert!(matches!(
            c.call(1, Command::Ping {}).await?,
            Response::Pong { .. }
        ));
        fx.shutdown().await
    }

    #[tokio::test]
    async fn refuses_to_replace_a_regular_file() -> TestResult {
        let fx = Fixture::new()?;
        let parent = fx.socket.parent().ok_or("no parent")?.to_path_buf();
        std::fs::create_dir_all(&parent)?;
        std::fs::write(&fx.socket, b"precious")?;
        let (_tx, rx) = oneshot::channel::<()>();
        let r = serve(Arc::clone(&fx.svc), &fx.socket, async {
            let _ = rx.await;
        })
        .await;
        assert!(r.is_err());
        assert_eq!(std::fs::read(&fx.socket)?, b"precious");
        Ok(())
    }

    #[tokio::test]
    async fn logs_follow_streams_until_disconnect() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;
        let Response::Server(s) = c
            .call(
                1,
                Command::Create {
                    name: "a".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: true,
                },
            )
            .await?
        else {
            return Err("expected a server".into());
        };
        c.call(2, Command::Start { id: s.id.clone() }).await?;

        let mut f = fx.client().await?;
        f.raw(&encode_line(&Request {
            id: Some(9),
            cmd: Command::LogsFollow { id: s.id.clone() },
        }))
        .await?;
        // The follow is registered once the runtime saw the call.
        for _ in 0..200 {
            if fx
                .rt
                .calls()
                .iter()
                .any(|c| matches!(c, crate::runtime::RuntimeCall::LogsFollow { .. }))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fx.rt.push_log(&s.id, "one");
        fx.rt.push_log(&s.id, "two");
        for expected in ["one", "two"] {
            let r = f.read_reply().await?;
            assert_eq!(r.id, Some(9));
            assert_eq!(r.response, Response::Line(expected.into()));
        }
        drop(f);

        let mut f = fx.client().await?;
        f.raw(&encode_line(&Request {
            id: None,
            cmd: Command::LogsFollow { id: "nope".into() },
        }))
        .await?;
        let r = f.read_reply().await?;
        assert_eq!(kind_of(&r.response), Some(ErrorKind::NotFound));
        fx.shutdown().await
    }

    #[tokio::test]
    async fn shutdown_ends_open_followers() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;
        let Response::Server(s) = c
            .call(
                1,
                Command::Create {
                    name: "a".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: true,
                },
            )
            .await?
        else {
            return Err("expected a server".into());
        };
        c.call(2, Command::Start { id: s.id.clone() }).await?;
        let mut f = fx.client().await?;
        f.raw(&encode_line(&Request {
            id: None,
            cmd: Command::LogsFollow { id: s.id },
        }))
        .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        fx.shutdown().await?;
        assert!(started.elapsed() < Duration::from_secs(5));
        Ok(())
    }

    #[tokio::test]
    async fn diagnose_contains_no_secrets() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;
        let Response::Server(s) = c
            .call(
                1,
                Command::Create {
                    name: "a".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: true,
                },
            )
            .await?
        else {
            return Err("expected a server".into());
        };
        let password = fx.svc.get_server(&s.id)?.rcon_password;
        assert!(!password.is_empty());
        let reply = c.call(2, Command::Diagnose {}).await?;
        let Response::Diagnose(d) = &reply else {
            return Err("expected diagnose".into());
        };
        assert_eq!(d.protocol, PROTOCOL_VERSION);
        assert_eq!(d.podman_version.as_deref(), Some("5.4.2-fake"));
        assert_eq!(d.servers.len(), 1);
        assert!(!d.events.is_empty());
        assert!(!serde_json::to_string(&reply)?.contains(&password));
        fx.shutdown().await
    }

    #[tokio::test]
    async fn performance_commands_over_the_socket() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;

        let r = c.call(1, Command::Resources { id: "nope".into() }).await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::NotFound));
        let r = c
            .call(2, Command::ViewDistanceGet { id: "nope".into() })
            .await?;
        assert_eq!(kind_of(&r), Some(ErrorKind::NotFound));

        let Response::Server(s) = c
            .call(
                3,
                Command::Create {
                    name: "a".into(),
                    mc_version: "26.3".into(),
                    eula_accepted: true,
                },
            )
            .await?
        else {
            return Err("expected a server".into());
        };

        // Nothing chosen yet: the limits come with the answer.
        let Response::ViewDistance(v) = c
            .call(4, Command::ViewDistanceGet { id: s.id.clone() })
            .await?
        else {
            return Err("expected a view distance".into());
        };
        assert_eq!((v.value, v.default_value, v.min, v.max), (None, 10, 4, 16));

        let Response::ViewDistance(v) = c
            .call(
                5,
                Command::ViewDistanceSet {
                    id: s.id.clone(),
                    value: 8,
                },
            )
            .await?
        else {
            return Err("expected a view distance".into());
        };
        assert_eq!(v.value, Some(8));

        // Out of range is refused and the value stays.
        for bad in [0, 3, 17, 255] {
            let r = c
                .call(
                    6,
                    Command::ViewDistanceSet {
                        id: s.id.clone(),
                        value: bad,
                    },
                )
                .await?;
            assert_eq!(kind_of(&r), Some(ErrorKind::InvalidInput), "{bad}");
        }
        let Response::ViewDistance(v) = c
            .call(7, Command::ViewDistanceGet { id: s.id.clone() })
            .await?
        else {
            return Err("expected a view distance".into());
        };
        assert_eq!(v.value, Some(8));

        // A stopped server: limits known, nothing in use, and no secret leaks.
        let reply = c.call(8, Command::Resources { id: s.id.clone() }).await?;
        let Response::Resources(info) = &reply else {
            return Err("expected resources".into());
        };
        assert!(!info.running);
        assert!(info.game.is_none());
        assert!(info.memory_limit_bytes.is_some());
        assert_eq!(info.disk.as_ref().map(|d| d.total_bytes), Some(100));
        fx.shutdown().await
    }

    #[tokio::test]
    async fn tunnel_commands_over_the_socket() -> TestResult {
        use craft_a_server_core::tunnel::TunnelState;
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;

        // Off by default.
        let Response::Tunnel(info) = c.call(1, Command::TunnelStatus {}).await? else {
            return Err("expected tunnel info".into());
        };
        assert!(!info.enabled);
        assert_eq!(info.state, TunnelState::NotConfigured);

        // Enabling without the confirmation is refused and changes nothing.
        let refused = c.call(2, Command::TunnelEnable { confirm: false }).await?;
        assert_eq!(kind_of(&refused), Some(ErrorKind::Tunnel), "{refused:?}");
        let Response::Tunnel(info) = c.call(3, Command::TunnelStatus {}).await? else {
            return Err("expected tunnel info".into());
        };
        assert!(!info.enabled);

        // With it, it turns on (the supervisor is not running in this test).
        let Response::Tunnel(info) = c.call(4, Command::TunnelEnable { confirm: true }).await?
        else {
            return Err("expected tunnel info".into());
        };
        assert!(info.enabled);
        assert_eq!(info.consent_unix, Some(1_700_000_000));
        assert_eq!(info.agent_version, "1.2.0");
        assert_eq!(info.local_port, 25565);

        let Response::Tunnel(info) = c
            .call(
                5,
                Command::TunnelDisable {
                    forget_claim: false,
                },
            )
            .await?
        else {
            return Err("expected tunnel info".into());
        };
        assert!(!info.enabled);

        // The audit log says what happened, without any secret.
        let events = fx.svc.db().recent_events(20)?;
        assert!(events.iter().any(|e| e.kind == "tunnel_enabled"));
        assert!(events.iter().any(|e| e.kind == "tunnel_disabled"));
        fx.shutdown().await
    }

    #[test]
    fn a_daemon_without_a_tunnel_answers_with_a_clear_error() -> TestResult {
        let e = ServiceError::Tunnel(TunnelError::NotAvailable);
        let Response::Error { kind, message } = error_response(&e) else {
            return Err("expected an error".into());
        };
        assert_eq!(kind, ErrorKind::Tunnel);
        assert!(message.contains("not set up"), "{message}");
        // Storage failures stay generic.
        let e = ServiceError::Tunnel(TunnelError::Io(io::Error::other("/secret/path")));
        let Response::Error { kind, message } = error_response(&e) else {
            return Err("expected an error".into());
        };
        assert_eq!(kind, ErrorKind::Internal);
        assert!(!message.contains("/secret/path"));
        Ok(())
    }

    #[tokio::test]
    async fn pairing_and_devices_over_the_socket() -> TestResult {
        let mut fx = Fixture::new()?;
        fx.serve().await?;
        let mut c = fx.client().await?;
        let Response::PairingCode {
            code,
            expires_unix,
            url,
            ..
        } = c.call(1, Command::Pair {}).await?
        else {
            return Err("expected a pairing code".into());
        };
        assert_eq!(code.len(), 8);
        assert!(expires_unix > 0);
        assert!(
            url.ends_with("/pair") && url.starts_with("http://"),
            "{url}"
        );
        // Only the hash is stored, and the code redeems exactly once.
        let now = fx.svc.now();
        assert_eq!(fx.svc.db().count_pairing_codes(now)?, 1);
        let db = fx.svc.db();
        let out = crate::panel::auth::redeem_code(db, &code, "Pixel", now, 3600)?
            .ok_or("code should redeem")?;
        assert!(crate::panel::auth::redeem_code(db, &code, "Pixel", now + 1, 3600)?.is_none());

        let Response::Devices(list) = c.call(2, Command::DevicesList {}).await? else {
            return Err("expected devices".into());
        };
        assert_eq!(list.len(), 1);
        let device = list.first().ok_or("one device")?;
        assert_eq!(device.device_name, "Pixel");
        assert_eq!(device.id, out.session.id);
        let wire = serde_json::to_string(&list)?;
        assert!(!wire.contains(&out.token) && !wire.contains(&out.session.csrf_token));

        let id = device.id.clone();
        assert!(matches!(
            c.call(3, Command::DevicesRevoke { id: id.clone() }).await?,
            Response::Ok
        ));
        assert_eq!(
            kind_of(&c.call(4, Command::DevicesRevoke { id }).await?),
            Some(ErrorKind::NotFound)
        );
        let Response::Devices(list) = c.call(5, Command::DevicesList {}).await? else {
            return Err("expected devices".into());
        };
        assert!(list.is_empty());
        fx.shutdown().await
    }

    #[test]
    fn peer_policy() {
        assert!(peer_allowed(1000, 1000));
        assert!(peer_allowed(0, 1000));
        assert!(!peer_allowed(1001, 1000));
    }

    #[test]
    fn internal_errors_do_not_leak_details() {
        let leaky = ServiceError::Io {
            context: "creating the data directory",
            source: io::Error::other("/var/lib/craft-a-server/servers/s1: disk on fire"),
        };
        let Response::Error { kind, message } = error_response(&leaky) else {
            return;
        };
        assert_eq!(kind, ErrorKind::Internal);
        assert!(!message.contains("/var/lib"));
        assert!(!message.contains("on fire"));

        let db = ServiceError::Db(crate::db::DbError::Corrupt("SELECT * FROM servers".into()));
        let Response::Error { message, .. } = error_response(&db) else {
            return;
        };
        assert!(!message.contains("SELECT"));
    }

    #[test]
    fn other_errors_map_to_their_kinds() {
        let cases: Vec<(ServiceError, ErrorKind)> = vec![
            (
                ServiceError::InvalidInput("x".into()),
                ErrorKind::InvalidInput,
            ),
            (ServiceError::NotFound("x".into()), ErrorKind::NotFound),
            (
                ServiceError::WrongState {
                    id: "s".into(),
                    state: "running".into(),
                },
                ErrorKind::WrongState,
            ),
            (ServiceError::EulaNotAccepted, ErrorKind::Eula),
            (
                ServiceError::StartFailed("x".into()),
                ErrorKind::StartFailed,
            ),
            (ServiceError::DiskLow, ErrorKind::DiskLow),
            (
                ServiceError::Memory(craft_a_server_core::memory::MemoryError::TooLittle {
                    available_mib: 1,
                }),
                ErrorKind::Memory,
            ),
            (
                ServiceError::Runtime(RuntimeError::NotFound("c".into())),
                ErrorKind::WrongState,
            ),
            (
                ServiceError::Runtime(RuntimeError::Parse("x".into())),
                ErrorKind::Internal,
            ),
        ];
        for (err, kind) in cases {
            assert_eq!(kind_of(&error_response(&err)), Some(kind));
        }
    }
}
