//! `craft-a-server`: the command line for the craft-a-server daemon.
//!
//! Exit codes: 0 success, 1 the daemon reported an error or the command was
//! refused locally, 2 the daemon is not reachable, 64 bad command line.

mod args;
mod client;
mod render;
mod update;

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use clap::Parser;
use craft_a_server_core::ctl::{Command, PROTOCOL_VERSION, Reply, Response, encode_line};

use args::{Cli, Cmd, eula_message, is_yes};
use client::{Client, ClientError, error_of};

const EXIT_ERROR: u8 = 1;
const EXIT_UNREACHABLE: u8 = 2;
const EXIT_USAGE: u8 = 64;
const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let usage = e.use_stderr();
            let _ = e.print();
            return if usage {
                ExitCode::from(EXIT_USAGE)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    ExitCode::from(run(&cli))
}

fn run(cli: &Cli) -> u8 {
    if let Cmd::Create {
        accept_eula: false, ..
    } = &cli.command
    {
        eprintln!("{}", eula_message());
        return EXIT_ERROR;
    }
    if let Some(warning) = cli.command.needs_confirmation()
        && !cli.yes
        && !confirm(&warning)
    {
        eprintln!("Cancelled.");
        return EXIT_ERROR;
    }
    if let Cmd::Update { action } = &cli.command {
        return update::command::run(&cli.config, cli.json, cli.yes, action);
    }
    if matches!(cli.command, Cmd::Diagnose) {
        return diagnose(cli);
    }

    let mut client = match Client::connect(&cli.socket) {
        Ok(c) => c,
        Err(e) => return unreachable(cli, &e),
    };
    if let Cmd::Logs {
        id, follow: true, ..
    } = &cli.command
    {
        return logs_follow(cli, &mut client, id);
    }
    if let Cmd::Resources { .. } = &cli.command {
        return resources(cli, &mut client);
    }
    if let Cmd::Mods { action } = &cli.command
        && let Some(plan) = args::plan_command(action)
    {
        return mods_with_preview(cli, &mut client, plan);
    }
    match client.request(cli.command.to_command()) {
        Ok(reply) => finish(cli, &reply),
        Err(e) => connection_failed(cli, &e),
    }
}

/// The processor figure is a rate between two readings, so a first reading
/// alone has none; take a second one a moment later instead of printing
/// "measuring" to a person who just asked.
fn resources(cli: &Cli, client: &mut Client) -> u8 {
    let mut reply = match client.request(cli.command.to_command()) {
        Ok(r) => r,
        Err(e) => return connection_failed(cli, &e),
    };
    let needs_second = matches!(
        &reply.response,
        Response::Resources(info)
            if info.game.as_ref().is_some_and(|g| g.cpu_millicores.is_none())
    );
    if needs_second {
        std::thread::sleep(std::time::Duration::from_secs(2));
        reply = match client.request(cli.command.to_command()) {
            Ok(r) => r,
            Err(e) => return connection_failed(cli, &e),
        };
    }
    finish(cli, &reply)
}

/// Asks on the terminal; anything but yes (including end of input) is a no.
fn confirm(warning: &str) -> bool {
    eprintln!("{warning}");
    eprint!("Continue? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer).is_ok() && is_yes(&answer)
}

/// Prints a reply and returns the exit code it deserves.
fn finish(cli: &Cli, reply: &Reply) -> u8 {
    if cli.json {
        print!("{}", encode_line(reply));
    } else if let Some((_, message)) = error_of(&reply.response) {
        eprintln!("error: {}", render::sanitize(message));
    } else {
        println!("{}", render_response(&cli.command, &reply.response));
    }
    if error_of(&reply.response).is_some() {
        EXIT_ERROR
    } else {
        0
    }
}

fn render_response(cmd: &Cmd, response: &Response) -> String {
    match (cmd, response) {
        (Cmd::Create { .. }, Response::Server(s)) => format!(
            "Created server {} ({}, Minecraft {}) on port {}.\nStart it with: craft-a-server start {}",
            render::sanitize(&s.id),
            render::sanitize(&s.name),
            render::sanitize(&s.mc_version),
            s.port,
            render::sanitize(&s.id)
        ),
        (Cmd::Start { .. } | Cmd::Restart { .. }, Response::Server(s)) => format!(
            "Server {} is {}; join it on port {}.",
            render::sanitize(&s.id),
            render::sanitize(&s.state),
            s.port
        ),
        (Cmd::Stop { .. } | Cmd::Reset { .. }, Response::Server(s)) => {
            format!(
                "Server {} is {}.",
                render::sanitize(&s.id),
                render::sanitize(&s.state)
            )
        }
        (Cmd::Delete { id }, Response::Ok) => format!("Deleted server {}.", render::sanitize(id)),
        (Cmd::Backup { .. }, Response::Ok) => "Backup restored.".to_owned(),
        (
            Cmd::Devices {
                action: args::DevicesCmd::Revoke { id },
            },
            Response::Ok,
        ) => format!("Signed out device {}.", render::sanitize(id)),
        (_, Response::Server(s)) => render::render_server(s),
        (_, Response::Servers(list)) => render::render_servers(list),
        (_, Response::Backups(list)) => render::render_backups(list),
        (_, Response::Backup(b)) => format!(
            "Backup {} created ({}, verified: {}).",
            render::sanitize(&b.id),
            render::format_size(b.size_bytes),
            if b.verified { "yes" } else { "no" }
        ),
        (_, Response::Text(text)) => render::sanitize_text(text),
        (_, Response::Line(text)) => render::sanitize(text),
        (
            _,
            Response::Pong {
                protocol,
                daemon_version,
            },
        ) => {
            format!(
                "craft-a-serverd {} (protocol {protocol})",
                render::sanitize(daemon_version)
            )
        }
        (
            _,
            Response::PairingCode {
                code,
                expires_unix,
                url,
                alt_urls,
            },
        ) => render::render_pairing(code, *expires_unix, now_unix(), url, alt_urls),
        (_, Response::Devices(list)) => render::render_devices(list),
        (_, Response::ModHits(list)) => render::render_mod_hits(list),
        (_, Response::ModList(list)) => render::render_mod_list(list),
        (_, Response::ModPlan(plan)) => render::render_mod_plan(plan),
        (_, Response::ModRemovePlan(plan)) => render::render_mod_remove_plan(plan),
        (_, Response::ModUpdates(list)) => render::render_mod_updates(list),
        (_, Response::ModChange(change)) => render::render_mod_change(change),
        (
            Cmd::Tunnel {
                action: args::TunnelCmd::ClaimUrl,
            },
            Response::Tunnel(info),
        ) => render::render_claim_url(info),
        (_, Response::Tunnel(info)) => render::render_tunnel(info),
        (_, Response::Resources(info)) => render::render_resources(info),
        (cmd, Response::ViewDistance(info)) => render::render_view_distance(
            info,
            matches!(cmd, Cmd::ViewDistance { value: Some(_), .. }),
        ),
        (_, Response::Diagnose(d)) => render::render_diagnose(d, CLI_VERSION, PROTOCOL_VERSION),
        (_, Response::Ok) => "Done.".to_owned(),
        (_, Response::Error { message, .. }) => render::sanitize(message),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn logs_follow(cli: &Cli, client: &mut Client, id: &str) -> u8 {
    // History first, then the live stream on a second connection.
    let tail = match client.request(cli.command.to_command()) {
        Ok(r) => r,
        Err(e) => return connection_failed(cli, &e),
    };
    let code = finish(cli, &tail);
    if code != 0 {
        return code;
    }
    let mut stream = match Client::connect(&cli.socket) {
        Ok(c) => c,
        Err(e) => return unreachable(cli, &e),
    };
    if let Err(e) = stream.send(Command::LogsFollow { id: id.to_owned() }) {
        return connection_failed(cli, &e);
    }
    loop {
        let reply = match stream.read_reply() {
            Ok(r) => r,
            Err(e) => return connection_failed(cli, &e),
        };
        match &reply.response {
            Response::Line(text) => {
                if cli.json {
                    print!("{}", encode_line(&reply));
                } else {
                    println!("{}", render::sanitize(text));
                }
                // Keep a pipe such as `| grep` responsive.
                let _ = io::stdout().flush();
            }
            Response::Ok => return 0,
            _ => return finish(cli, &reply),
        }
    }
}

/// Shows what an install or removal will do, asks, then runs it.
fn mods_with_preview(cli: &Cli, client: &mut Client, plan: Command) -> u8 {
    let preview = match client.request(plan) {
        Ok(r) => r,
        Err(e) => return connection_failed(cli, &e),
    };
    if error_of(&preview.response).is_some() {
        return finish(cli, &preview);
    }
    if !cli.json {
        println!("{}", render_response(&cli.command, &preview.response));
    }
    // Conflicts mean the daemon will refuse; no point asking.
    let blocked = matches!(&preview.response, Response::ModPlan(p) if !p.conflicts.is_empty());
    if !blocked && !cli.yes && !confirm("This changes the server's mods.") {
        eprintln!("Cancelled.");
        return EXIT_ERROR;
    }
    match client.request(cli.command.to_command()) {
        Ok(reply) => finish(cli, &reply),
        Err(e) => connection_failed(cli, &e),
    }
}

fn diagnose(cli: &Cli) -> u8 {
    let mut client = match Client::connect(&cli.socket) {
        Ok(c) => c,
        Err(e) => return unreachable(cli, &e),
    };
    match client.request(Command::Diagnose {}) {
        Ok(reply) => finish(cli, &reply),
        Err(e) => connection_failed(cli, &e),
    }
}

/// The daemon could not be reached: say so, exit 2.
fn unreachable(cli: &Cli, error: &ClientError) -> u8 {
    let socket = cli.socket.display().to_string();
    if cli.json {
        let value = serde_json::json!({
            "reachable": false,
            "socket": socket,
            "error": error.to_string(),
        });
        println!("{value}");
    } else if matches!(cli.command, Cmd::Diagnose) {
        println!(
            "{}",
            render::render_unreachable(&socket, &error.to_string(), CLI_VERSION, PROTOCOL_VERSION)
        );
    } else {
        eprintln!(
            "error: daemon not reachable at {socket}: {error}\n\
             Is craft-a-serverd running? Try: systemctl status craft-a-serverd"
        );
    }
    EXIT_UNREACHABLE
}

fn connection_failed(cli: &Cli, error: &ClientError) -> u8 {
    match error {
        ClientError::Unreachable(_) => unreachable(cli, error),
        ClientError::Io(_) | ClientError::Protocol(_) => {
            eprintln!("error: lost the connection to the daemon: {error}");
            EXIT_ERROR
        }
        ClientError::Ambiguous(_) => {
            eprintln!("error: {error}");
            EXIT_ERROR
        }
    }
}
