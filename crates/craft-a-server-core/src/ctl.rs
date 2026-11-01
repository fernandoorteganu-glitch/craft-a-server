//! Control protocol between the CLI and the daemon.
//!
//! Newline-delimited JSON over a Unix socket. One request per line, answered by
//! one reply line, except `logs_follow`, which answers with many `line` replies
//! until the client disconnects.
//!
//! ```text
//! -> {"id":1,"cmd":{"type":"start","id":"s0a1b2c3d"}}
//! <- {"id":1,"response":{"type":"server","data":{"id":"s0a1b2c3d", ...}}}
//! ```
//!
//! The protocol is deliberately strict: unknown fields and unknown commands are
//! rejected with a clear message, and a request line is capped at
//! [`MAX_REQUEST_BYTES`]. Bump [`PROTOCOL_VERSION`] on any incompatible change.

use serde::{Deserialize, Serialize};

/// Version reported by `ping`; the CLI warns when it differs from its own.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest accepted request line, including the newline.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Longest reply line a client accepts (a list of servers or events can be large).
pub const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;

/// Console actions the daemon may relay to a server; no free text exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleAction {
    WhitelistAdd,
    WhitelistRemove,
    Kick,
    WhitelistList,
}

/// What the client asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Ping {},
    List {},
    Create {
        name: String,
        mc_version: String,
        eula_accepted: bool,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Restart {
        id: String,
    },
    Reset {
        id: String,
    },
    Delete {
        id: String,
    },
    Status {
        id: String,
    },
    BackupCreate {
        id: String,
    },
    BackupList {
        id: String,
    },
    BackupRestore {
        id: String,
        backup_id: String,
    },
    Console {
        id: String,
        action: ConsoleAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        player: Option<String>,
    },
    LogsTail {
        id: String,
        lines: u32,
    },
    LogsFollow {
        id: String,
    },
    Diagnose {},
    /// Create a one-time pairing code for the web panel.
    Pair {},
    /// List paired devices (phones signed in to the panel).
    DevicesList {},
    /// Sign a paired device out, by its public id.
    DevicesRevoke {
        id: String,
    },
}

/// One request line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Echoed in the reply so a client can match them up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub cmd: Command,
}

/// Stable machine-readable failure classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    NotFound,
    WrongState,
    Eula,
    StartFailed,
    DiskLow,
    Memory,
    Internal,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::NotFound => "not_found",
            ErrorKind::WrongState => "wrong_state",
            ErrorKind::Eula => "eula",
            ErrorKind::StartFailed => "start_failed",
            ErrorKind::DiskLow => "disk_low",
            ErrorKind::Memory => "memory",
            ErrorKind::Internal => "internal",
        }
    }
}

/// A server as shown to clients. Never carries secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInfo {
    pub id: String,
    pub name: String,
    pub mc_version: String,
    /// Stable state name from `ServerState::to_parts` (`running`, `failed`, ...).
    pub state: String,
    /// Failure reason when `state` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub port: u16,
    pub created_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupInfo {
    pub id: String,
    pub server_id: String,
    /// `daily`, `manual`, `pre_change` or `pre_restore`.
    pub kind: String,
    pub size_bytes: u64,
    pub created_unix: i64,
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventInfo {
    pub ts_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    pub kind: String,
    pub detail: String,
}

/// What `diagnose` reports. Contains no secrets (no RCON passwords).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnoseInfo {
    pub daemon_version: String,
    pub protocol: u32,
    /// `None` when podman could not be queried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub podman_version: Option<String>,
    pub schema_version: u32,
    pub servers: Vec<ServerInfo>,
    /// Newest first, at most 20.
    pub events: Vec<EventInfo>,
}

/// A phone paired with the panel. Never carries the session token or CSRF token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInfo {
    /// Public id, used to revoke.
    pub id: String,
    pub device_name: String,
    pub created_unix: i64,
    pub last_seen_unix: i64,
    pub expires_unix: i64,
}

/// What the daemon answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    Pong {
        protocol: u32,
        daemon_version: String,
    },
    Servers(Vec<ServerInfo>),
    Server(ServerInfo),
    Backups(Vec<BackupInfo>),
    Backup(BackupInfo),
    Diagnose(DiagnoseInfo),
    Text(String),
    /// One log line of a `logs_follow` stream.
    Line(String),
    Ok,
    /// A fresh pairing code. `code` is the raw 8 characters (no dash).
    PairingCode {
        code: String,
        expires_unix: i64,
        /// Pairing page by hostname.
        url: String,
        /// The same page by LAN address, for networks without mDNS.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        alt_urls: Vec<String>,
    },
    Devices(Vec<DeviceInfo>),
    Error {
        kind: ErrorKind,
        message: String,
    },
}

/// One reply line: the response plus the request id it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub response: Response,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("request is larger than {MAX_REQUEST_BYTES} bytes")]
    TooLarge,
    #[error("empty request")]
    Empty,
    #[error("malformed request: {0}")]
    Malformed(String),
}

/// Parses one request line (with or without the trailing newline).
pub fn parse_request(line: &str) -> Result<Request, ProtocolError> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    let line = line.trim();
    if line.is_empty() {
        return Err(ProtocolError::Empty);
    }
    serde_json::from_str(line).map_err(|e| ProtocolError::Malformed(e.to_string()))
}

/// Parses one reply line.
pub fn parse_reply(line: &str) -> Result<Reply, ProtocolError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(ProtocolError::Empty);
    }
    serde_json::from_str(line).map_err(|e| ProtocolError::Malformed(e.to_string()))
}

/// Serializes a request or reply as one newline-terminated line.
///
/// Our types always serialize; the fallback keeps the signature infallible.
pub fn encode_line<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string(value)
        .unwrap_or_else(|_| r#"{"response":{"type":"error","data":{"kind":"internal","message":"encoding failed"}}}"#.to_owned());
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn all_commands() -> Vec<Command> {
        let id = || "s1".to_owned();
        vec![
            Command::Ping {},
            Command::List {},
            Command::Create {
                name: "Survival".into(),
                mc_version: "26.3".into(),
                eula_accepted: true,
            },
            Command::Start { id: id() },
            Command::Stop { id: id() },
            Command::Restart { id: id() },
            Command::Reset { id: id() },
            Command::Delete { id: id() },
            Command::Status { id: id() },
            Command::BackupCreate { id: id() },
            Command::BackupList { id: id() },
            Command::BackupRestore {
                id: id(),
                backup_id: "b1".into(),
            },
            Command::Console {
                id: id(),
                action: ConsoleAction::WhitelistAdd,
                player: Some("Steve".into()),
            },
            Command::Console {
                id: id(),
                action: ConsoleAction::WhitelistList,
                player: None,
            },
            Command::LogsTail {
                id: id(),
                lines: 50,
            },
            Command::LogsFollow { id: id() },
            Command::Diagnose {},
            Command::Pair {},
            Command::DevicesList {},
            Command::DevicesRevoke { id: id() },
        ]
    }

    #[test]
    fn requests_roundtrip() -> TestResult {
        for (n, cmd) in all_commands().into_iter().enumerate() {
            let req = Request {
                id: Some(n as u64),
                cmd,
            };
            let line = encode_line(&req);
            assert!(line.ends_with('\n'));
            assert_eq!(line.matches('\n').count(), 1);
            assert_eq!(parse_request(&line)?, req);
        }
        Ok(())
    }

    #[test]
    fn request_wire_format_is_stable() -> TestResult {
        let req = Request {
            id: Some(7),
            cmd: Command::Start { id: "s1".into() },
        };
        assert_eq!(
            encode_line(&req),
            "{\"id\":7,\"cmd\":{\"type\":\"start\",\"id\":\"s1\"}}\n"
        );
        let no_id = parse_request(r#"{"cmd":{"type":"ping"}}"#)?;
        assert_eq!(no_id.id, None);
        assert_eq!(no_id.cmd, Command::Ping {});
        Ok(())
    }

    #[test]
    fn replies_roundtrip() -> TestResult {
        let server = ServerInfo {
            id: "s1".into(),
            name: "Survival".into(),
            mc_version: "26.3".into(),
            state: "failed".into(),
            reason: Some("boom".into()),
            port: 25565,
            created_unix: 1_700_000_000,
        };
        let backup = BackupInfo {
            id: "b1".into(),
            server_id: "s1".into(),
            kind: "manual".into(),
            size_bytes: 12,
            created_unix: 1,
            verified: true,
        };
        let responses = vec![
            Response::Pong {
                protocol: PROTOCOL_VERSION,
                daemon_version: "0.0.1".into(),
            },
            Response::Servers(vec![server.clone()]),
            Response::Server(server.clone()),
            Response::Backups(vec![backup.clone()]),
            Response::Backup(backup),
            Response::Diagnose(DiagnoseInfo {
                daemon_version: "0.0.1".into(),
                protocol: PROTOCOL_VERSION,
                podman_version: None,
                schema_version: 1,
                servers: vec![server],
                events: vec![EventInfo {
                    ts_unix: 5,
                    server_id: None,
                    kind: "reconciled".into(),
                    detail: "x".into(),
                }],
            }),
            Response::Text("hello".into()),
            Response::Line("[12:00] hi".into()),
            Response::Ok,
            Response::PairingCode {
                code: "ABCD2345".into(),
                expires_unix: 99,
                url: "http://craft-a-server.local:8080/pair".into(),
                alt_urls: vec!["http://192.168.1.5:8080/pair".into()],
            },
            Response::Devices(vec![DeviceInfo {
                id: "d1".into(),
                device_name: "Pixel".into(),
                created_unix: 1,
                last_seen_unix: 2,
                expires_unix: 3,
            }]),
            Response::Error {
                kind: ErrorKind::WrongState,
                message: "nope".into(),
            },
        ];
        for response in responses {
            let reply = Reply {
                id: Some(3),
                response,
            };
            assert_eq!(parse_reply(&encode_line(&reply))?, reply);
        }
        Ok(())
    }

    #[test]
    fn pairing_commands_have_stable_wire_names() -> TestResult {
        assert_eq!(
            encode_line(&Request {
                id: None,
                cmd: Command::Pair {}
            }),
            "{\"cmd\":{\"type\":\"pair\"}}
"
        );
        assert_eq!(
            parse_request(r#"{"cmd":{"type":"devices_revoke","id":"d1"}}"#)?.cmd,
            Command::DevicesRevoke { id: "d1".into() }
        );
        assert!(parse_request(r#"{"cmd":{"type":"pair","x":1}}"#).is_err());
        Ok(())
    }

    #[test]
    fn error_kinds_serialize_to_their_stable_names() -> TestResult {
        for kind in [
            ErrorKind::InvalidInput,
            ErrorKind::NotFound,
            ErrorKind::WrongState,
            ErrorKind::Eula,
            ErrorKind::StartFailed,
            ErrorKind::DiskLow,
            ErrorKind::Memory,
            ErrorKind::Internal,
        ] {
            assert_eq!(
                serde_json::to_string(&kind)?,
                format!("\"{}\"", kind.as_str())
            );
        }
        Ok(())
    }

    #[test]
    fn unknown_command_is_rejected_clearly() {
        let err = parse_request(r#"{"cmd":{"type":"format_disk"}}"#).err();
        let text = err.map(|e| e.to_string()).unwrap_or_default();
        assert!(text.contains("unknown variant"), "{text}");
        assert!(text.contains("format_disk"), "{text}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        for line in [
            r#"{"cmd":{"type":"start","id":"s1","force":true}}"#,
            r#"{"cmd":{"type":"ping","extra":1}}"#,
            r#"{"cmd":{"type":"list"},"sudo":true}"#,
        ] {
            let text = parse_request(line)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(text.contains("unknown field"), "{line}: {text}");
        }
    }

    #[test]
    fn missing_and_mistyped_fields_are_rejected() {
        for line in [
            r#"{"cmd":{"type":"start"}}"#,
            r#"{"cmd":{"type":"logs_tail","id":"s1","lines":"many"}}"#,
            r#"{"cmd":{"type":"console","id":"s1","action":"op"}}"#,
            r#"{"id":"x","cmd":{"type":"ping"}}"#,
            r#"{}"#,
            "not json",
        ] {
            assert!(
                matches!(parse_request(line), Err(ProtocolError::Malformed(_))),
                "{line}"
            );
        }
    }

    #[test]
    fn empty_and_oversized_lines_are_rejected() {
        assert_eq!(parse_request("  \n"), Err(ProtocolError::Empty));
        let huge = format!(
            r#"{{"cmd":{{"type":"start","id":"{}"}}}}"#,
            "a".repeat(MAX_REQUEST_BYTES)
        );
        assert_eq!(parse_request(&huge), Err(ProtocolError::TooLarge));
    }
}
