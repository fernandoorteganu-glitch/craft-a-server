//! Command-line arguments and their translation to protocol commands.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use craft_a_server_core::ctl::{Command, ConsoleAction};

pub const DEFAULT_SOCKET: &str = "/run/craft-a-server/ctl.sock";
pub const EULA_URL: &str = "https://aka.ms/MinecraftEULA";

#[derive(Debug, Parser)]
#[command(
    name = "craft-a-server",
    version,
    about = "Manage Minecraft servers on your craft-a-server mini PC"
)]
pub struct Cli {
    /// Control socket of the daemon.
    #[arg(
        long,
        global = true,
        env = "CRAFT_A_SERVER_SOCKET",
        default_value = DEFAULT_SOCKET,
        value_name = "PATH"
    )]
    pub socket: PathBuf,

    /// Print the daemon's raw JSON replies instead of formatted text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Do not ask for confirmation (delete, backup restore).
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List servers.
    List,
    /// Create a server (requires accepting the Minecraft EULA).
    Create {
        /// Display name.
        name: String,
        /// Minecraft version, for example 26.3.
        #[arg(long = "version", value_name = "MC_VERSION")]
        mc_version: String,
        /// Accept the Minecraft EULA (https://aka.ms/MinecraftEULA).
        #[arg(long)]
        accept_eula: bool,
    },
    /// Start a server and wait until it answers.
    Start { id: String },
    /// Stop a server cleanly.
    Stop { id: String },
    /// Stop and start a server.
    Restart { id: String },
    /// Acknowledge a failed server so it can be started again.
    Reset { id: String },
    /// Delete a stopped server, its world and its backups.
    Delete { id: String },
    /// Show one server, or all of them without an id.
    Status { id: Option<String> },
    /// Show a server's log.
    Logs {
        id: String,
        /// How many recent lines to show.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: u32,
        /// Keep streaming new lines (Ctrl-C to stop).
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Create, list and restore backups.
    Backup {
        #[command(subcommand)]
        action: BackupCmd,
    },
    /// Manage the whitelist of a running server.
    Whitelist {
        #[command(subcommand)]
        action: WhitelistCmd,
    },
    /// Kick a player from a running server.
    Kick { id: String, player: String },
    /// Print versions, server states and recent events.
    Diagnose,
    /// Show a one-time code to pair a phone with the web panel.
    Pair,
    /// List and sign out the phones paired with the web panel.
    Devices {
        #[command(subcommand)]
        action: DevicesCmd,
    },
    /// Find, install, remove and check mods from Modrinth.
    Mods {
        #[command(subcommand)]
        action: ModsCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModsCmd {
    /// Search Fabric mods that run on the server.
    Search {
        id: String,
        query: String,
        /// Also list mods whose server support is not stated.
        #[arg(long)]
        include_unknown: bool,
        /// Skip this many results (paging).
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// List installed mods and unmanaged jars.
    List { id: String },
    /// Install a mod (id or slug) with its required dependencies. Takes a
    /// backup first and rolls back if the server does not come up.
    Install {
        id: String,
        project: String,
        /// Allow beta and alpha builds when no release exists.
        #[arg(long)]
        allow_prerelease: bool,
    },
    /// Remove an installed mod (id from `mods list`).
    Remove {
        id: String,
        project: String,
        /// Remove even though other mods need it.
        #[arg(long)]
        force: bool,
        /// Also remove dependencies nothing else needs.
        #[arg(long)]
        remove_orphans: bool,
    },
    /// Check installed mods for newer releases (changes nothing).
    Check { id: String },
}

#[derive(Debug, Subcommand)]
pub enum DevicesCmd {
    /// List paired phones.
    List,
    /// Sign a phone out (use the id from `devices list`).
    Revoke { id: String },
}

#[derive(Debug, Subcommand)]
pub enum BackupCmd {
    /// Take a verified backup now.
    Create { id: String },
    /// List backups, newest first.
    List { id: String },
    /// Replace the server's data with a backup (the current state is saved first).
    Restore { id: String, backup_id: String },
}

#[derive(Debug, Subcommand)]
pub enum WhitelistCmd {
    Add { id: String, player: String },
    Remove { id: String, player: String },
    List { id: String },
}

impl Cmd {
    /// The protocol command for this subcommand.
    pub fn to_command(&self) -> Command {
        match self {
            Cmd::List | Cmd::Status { id: None } => Command::List {},
            Cmd::Create {
                name,
                mc_version,
                accept_eula,
            } => Command::Create {
                name: name.clone(),
                mc_version: mc_version.clone(),
                eula_accepted: *accept_eula,
            },
            Cmd::Start { id } => Command::Start { id: id.clone() },
            Cmd::Stop { id } => Command::Stop { id: id.clone() },
            Cmd::Restart { id } => Command::Restart { id: id.clone() },
            Cmd::Reset { id } => Command::Reset { id: id.clone() },
            Cmd::Delete { id } => Command::Delete { id: id.clone() },
            Cmd::Status { id: Some(id) } => Command::Status { id: id.clone() },
            Cmd::Logs {
                id,
                lines,
                follow: _,
            } => Command::LogsTail {
                id: id.clone(),
                lines: *lines,
            },
            Cmd::Backup { action } => match action {
                BackupCmd::Create { id } => Command::BackupCreate { id: id.clone() },
                BackupCmd::List { id } => Command::BackupList { id: id.clone() },
                BackupCmd::Restore { id, backup_id } => Command::BackupRestore {
                    id: id.clone(),
                    backup_id: backup_id.clone(),
                },
            },
            Cmd::Whitelist { action } => match action {
                WhitelistCmd::Add { id, player } => {
                    console(id, ConsoleAction::WhitelistAdd, player)
                }
                WhitelistCmd::Remove { id, player } => {
                    console(id, ConsoleAction::WhitelistRemove, player)
                }
                WhitelistCmd::List { id } => Command::Console {
                    id: id.clone(),
                    action: ConsoleAction::WhitelistList,
                    player: None,
                },
            },
            Cmd::Kick { id, player } => console(id, ConsoleAction::Kick, player),
            Cmd::Diagnose => Command::Diagnose {},
            Cmd::Pair => Command::Pair {},
            Cmd::Devices { action } => match action {
                DevicesCmd::List => Command::DevicesList {},
                DevicesCmd::Revoke { id } => Command::DevicesRevoke { id: id.clone() },
            },
            Cmd::Mods { action } => match action {
                ModsCmd::Search {
                    id,
                    query,
                    include_unknown,
                    offset,
                } => Command::ModsSearch {
                    id: id.clone(),
                    query: query.clone(),
                    include_unknown: *include_unknown,
                    offset: *offset,
                },
                ModsCmd::List { id } => Command::ModsList { id: id.clone() },
                ModsCmd::Install {
                    id,
                    project,
                    allow_prerelease,
                } => Command::ModsInstall {
                    id: id.clone(),
                    project: project.clone(),
                    allow_prerelease: *allow_prerelease,
                },
                ModsCmd::Remove {
                    id,
                    project,
                    force,
                    remove_orphans,
                } => Command::ModsRemove {
                    id: id.clone(),
                    project: project.clone(),
                    force: *force,
                    remove_orphans: *remove_orphans,
                },
                ModsCmd::Check { id } => Command::ModsCheck { id: id.clone() },
            },
        }
    }

    /// Whether the user must confirm before this runs.
    pub fn needs_confirmation(&self) -> Option<String> {
        match self {
            Cmd::Delete { id } => Some(format!(
                "This permanently deletes server {id}, its world and all its backups."
            )),
            Cmd::Backup {
                action: BackupCmd::Restore { id, backup_id },
            } => Some(format!(
                "This replaces the data of server {id} with backup {backup_id} \
                 (the current state is backed up first)."
            )),
            _ => None,
        }
    }
}

fn console(id: &str, action: ConsoleAction, player: &str) -> Command {
    Command::Console {
        id: id.to_owned(),
        action,
        player: Some(player.to_owned()),
    }
}

/// The read-only "what would happen" request that precedes an install or removal.
pub fn plan_command(action: &ModsCmd) -> Option<Command> {
    match action {
        ModsCmd::Install {
            id,
            project,
            allow_prerelease,
        } => Some(Command::ModsPlan {
            id: id.clone(),
            project: project.clone(),
            allow_prerelease: *allow_prerelease,
        }),
        ModsCmd::Remove { id, project, .. } => Some(Command::ModsRemovePlan {
            id: id.clone(),
            project: project.clone(),
        }),
        _ => None,
    }
}

/// The message shown when `create` runs without `--accept-eula`.
pub fn eula_message() -> String {
    format!(
        "Creating a server means accepting the Minecraft EULA.\n\
         Read it at {EULA_URL} and, if you agree, run the command again with --accept-eula."
    )
}

/// True for "y" / "yes" in any case.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut full = vec!["craft-a-server"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full)
    }

    #[test]
    fn defaults() -> TestResult {
        let cli = parse(&["list"])?;
        assert!(!cli.json && !cli.yes);
        assert!(matches!(cli.command, Cmd::List));
        assert_eq!(cli.command.to_command(), Command::List {});
        Ok(())
    }

    #[test]
    fn create_parses_version_and_eula() -> TestResult {
        let cli = parse(&["create", "My World", "--version", "26.3", "--accept-eula"])?;
        assert_eq!(
            cli.command.to_command(),
            Command::Create {
                name: "My World".into(),
                mc_version: "26.3".into(),
                eula_accepted: true
            }
        );
        let cli = parse(&["create", "x", "--version", "26.3"])?;
        assert!(matches!(
            cli.command.to_command(),
            Command::Create {
                eula_accepted: false,
                ..
            }
        ));
        assert!(parse(&["create", "x"]).is_err());
        Ok(())
    }

    #[test]
    fn globals_work_after_the_subcommand() -> TestResult {
        let cli = parse(&["delete", "s1", "--yes", "--json", "--socket", "/tmp/x.sock"])?;
        assert!(cli.yes && cli.json);
        assert_eq!(cli.socket, PathBuf::from("/tmp/x.sock"));
        Ok(())
    }

    #[test]
    fn simple_commands_map() -> TestResult {
        let id = || "s1".to_owned();
        for (args, want) in [
            (vec!["start", "s1"], Command::Start { id: id() }),
            (vec!["stop", "s1"], Command::Stop { id: id() }),
            (vec!["restart", "s1"], Command::Restart { id: id() }),
            (vec!["reset", "s1"], Command::Reset { id: id() }),
            (vec!["delete", "s1"], Command::Delete { id: id() }),
            (vec!["status"], Command::List {}),
            (vec!["status", "s1"], Command::Status { id: id() }),
            (vec!["diagnose"], Command::Diagnose {}),
            (vec!["pair"], Command::Pair {}),
            (vec!["devices", "list"], Command::DevicesList {}),
            (
                vec!["devices", "revoke", "d1"],
                Command::DevicesRevoke { id: "d1".into() },
            ),
            (
                vec!["backup", "create", "s1"],
                Command::BackupCreate { id: id() },
            ),
            (
                vec!["backup", "list", "s1"],
                Command::BackupList { id: id() },
            ),
            (
                vec!["backup", "restore", "s1", "b1"],
                Command::BackupRestore {
                    id: id(),
                    backup_id: "b1".into(),
                },
            ),
            (
                vec!["logs", "s1"],
                Command::LogsTail {
                    id: id(),
                    lines: 100,
                },
            ),
            (
                vec!["logs", "s1", "-n", "5", "-f"],
                Command::LogsTail { id: id(), lines: 5 },
            ),
        ] {
            assert_eq!(parse(&args)?.command.to_command(), want, "{args:?}");
        }
        Ok(())
    }

    #[test]
    fn console_commands_map() -> TestResult {
        let want = |action, player: Option<&str>| Command::Console {
            id: "s1".into(),
            action,
            player: player.map(str::to_owned),
        };
        assert_eq!(
            parse(&["whitelist", "add", "s1", "Steve"])?
                .command
                .to_command(),
            want(ConsoleAction::WhitelistAdd, Some("Steve"))
        );
        assert_eq!(
            parse(&["whitelist", "remove", "s1", "Steve"])?
                .command
                .to_command(),
            want(ConsoleAction::WhitelistRemove, Some("Steve"))
        );
        assert_eq!(
            parse(&["whitelist", "list", "s1"])?.command.to_command(),
            want(ConsoleAction::WhitelistList, None)
        );
        assert_eq!(
            parse(&["kick", "s1", "Steve"])?.command.to_command(),
            want(ConsoleAction::Kick, Some("Steve"))
        );
        assert!(parse(&["whitelist", "add", "s1"]).is_err());
        assert!(parse(&["kick", "s1"]).is_err());
        Ok(())
    }

    #[test]
    fn confirmation_only_for_destructive_commands() -> TestResult {
        assert!(
            parse(&["delete", "s1"])?
                .command
                .needs_confirmation()
                .is_some()
        );
        assert!(
            parse(&["backup", "restore", "s1", "b1"])?
                .command
                .needs_confirmation()
                .is_some()
        );
        assert!(
            parse(&["stop", "s1"])?
                .command
                .needs_confirmation()
                .is_none()
        );
        assert!(
            parse(&["backup", "create", "s1"])?
                .command
                .needs_confirmation()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn mods_commands_map() -> TestResult {
        assert_eq!(
            parse(&["mods", "search", "s1", "lithium", "--include-unknown"])?
                .command
                .to_command(),
            Command::ModsSearch {
                id: "s1".into(),
                query: "lithium".into(),
                include_unknown: true,
                offset: 0
            }
        );
        assert_eq!(
            parse(&["mods", "install", "s1", "sodium", "--allow-prerelease"])?
                .command
                .to_command(),
            Command::ModsInstall {
                id: "s1".into(),
                project: "sodium".into(),
                allow_prerelease: true
            }
        );
        assert!(matches!(
            parse(&["mods", "remove", "s1", "P7dR8mSH", "--force"])?
                .command
                .to_command(),
            Command::ModsRemove {
                force: true,
                remove_orphans: false,
                ..
            }
        ));
        assert_eq!(
            parse(&["mods", "check", "s1"])?.command.to_command(),
            Command::ModsCheck { id: "s1".into() }
        );
        assert_eq!(
            parse(&["mods", "list", "s1"])?.command.to_command(),
            Command::ModsList { id: "s1".into() }
        );
        assert!(parse(&["mods", "install", "s1"]).is_err());
        Ok(())
    }

    #[test]
    fn answers() {
        assert!(is_yes("y") && is_yes(" YES\n"));
        assert!(!is_yes("") && !is_yes("no") && !is_yes("yeah"));
    }

    #[test]
    fn eula_message_has_the_url() {
        let m = eula_message();
        assert!(m.contains("https://aka.ms/MinecraftEULA"));
        assert!(m.contains("--accept-eula"));
    }

    #[test]
    fn unknown_subcommand_is_an_error() {
        assert!(parse(&["format-disk"]).is_err());
    }
}
