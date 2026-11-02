//! Blocking client for the daemon's control socket.

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use craft_a_server_core::ctl::{
    Command, MAX_REPLY_BYTES, Reply, Request, Response, ServerInfo, encode_line, parse_reply,
};

#[derive(Debug)]
pub enum ClientError {
    /// The socket could not be opened: the daemon is down or not allowed.
    Unreachable(io::Error),
    /// The connection broke after it was established.
    Io(io::Error),
    /// The daemon said something we cannot understand.
    Protocol(String),
    /// The server name that was typed matches more than one server.
    Ambiguous(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unreachable(e) | ClientError::Io(e) => write!(f, "{e}"),
            ClientError::Protocol(m) => write!(f, "unexpected reply from the daemon: {m}"),
            ClientError::Ambiguous(name) => write!(
                f,
                "more than one server is called \"{name}\"; use its id (see `craft-a-server list`)"
            ),
        }
    }
}

/// The server reference (an id, or a name typed by a person) inside a command.
/// Other ids, such as a paired device's, are not server references.
fn server_ref_mut(cmd: &mut Command) -> Option<&mut String> {
    match cmd {
        Command::Start { id }
        | Command::Stop { id }
        | Command::Restart { id }
        | Command::Reset { id }
        | Command::Delete { id }
        | Command::Status { id }
        | Command::Resources { id }
        | Command::ViewDistanceGet { id }
        | Command::ViewDistanceSet { id, .. }
        | Command::BackupCreate { id }
        | Command::BackupList { id }
        | Command::BackupRestore { id, .. }
        | Command::Console { id, .. }
        | Command::LogsTail { id, .. }
        | Command::LogsFollow { id }
        | Command::ModsSearch { id, .. }
        | Command::ModsList { id }
        | Command::ModsPlan { id, .. }
        | Command::ModsInstall { id, .. }
        | Command::ModsRemovePlan { id, .. }
        | Command::ModsRemove { id, .. }
        | Command::ModsCheck { id, .. } => Some(id),
        _ => None,
    }
}

/// Which server a typed reference means: an exact id always wins, then a name
/// that exactly one server has. `Ok(None)` means nothing matched (the daemon
/// then reports "no such server"); several name matches are ambiguous.
fn pick_server(servers: &[ServerInfo], reference: &str) -> Result<Option<String>, ClientError> {
    if servers.iter().any(|s| s.id == reference) {
        return Ok(Some(reference.to_owned()));
    }
    let mut named = servers.iter().filter(|s| s.name == reference);
    match (named.next(), named.next()) {
        (Some(only), None) => Ok(Some(only.id.clone())),
        (Some(_), Some(_)) => Err(ClientError::Ambiguous(reference.to_owned())),
        (None, _) => Ok(None),
    }
}

impl std::error::Error for ClientError {}

/// One connection. Requests are sent one at a time.
#[allow(missing_debug_implementations)] // wraps trait objects
pub struct Client {
    reader: Box<dyn BufRead>,
    writer: Box<dyn Write>,
    next_id: u64,
}

impl Client {
    pub fn connect(socket: &Path) -> Result<Client, ClientError> {
        let (reader, writer) = open(socket).map_err(ClientError::Unreachable)?;
        Ok(Client::from_parts(reader, writer))
    }

    pub fn from_parts(reader: Box<dyn BufRead>, writer: Box<dyn Write>) -> Client {
        Client {
            reader,
            writer,
            next_id: 1,
        }
    }

    /// Sends `cmd` and returns the reply that answers it.
    pub fn request(&mut self, cmd: Command) -> Result<Reply, ClientError> {
        self.send(cmd)?;
        self.read_reply()
    }

    /// Sends `cmd` (a streaming command) without reading anything yet. A server
    /// given by name is first turned into its id.
    pub fn send(&mut self, mut cmd: Command) -> Result<(), ClientError> {
        if let Some(reference) = server_ref_mut(&mut cmd) {
            let wanted = reference.clone();
            self.send_raw(Command::List {})?;
            if let Response::Servers(servers) = self.read_reply()?.response
                && let Some(id) = pick_server(&servers, &wanted)?
            {
                *reference = id;
            }
        }
        self.send_raw(cmd)
    }

    fn send_raw(&mut self, cmd: Command) -> Result<(), ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = encode_line(&Request { id: Some(id), cmd });
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(ClientError::Io)
    }

    /// Reads the next reply line.
    pub fn read_reply(&mut self) -> Result<Reply, ClientError> {
        let line = read_line_bounded(&mut self.reader, MAX_REPLY_BYTES)
            .map_err(ClientError::Io)?
            .ok_or_else(|| {
                ClientError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the daemon closed the connection",
                ))
            })?;
        parse_reply(&line).map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

/// Reads one line of at most `max` bytes; `None` at end of input.
pub fn read_line_bounded<R: BufRead + ?Sized>(
    reader: &mut R,
    max: usize,
) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                to_string(line).map(Some)
            };
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let take = newline.map_or(chunk.len(), |i| i + 1);
        if line.len() + take > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reply line too long",
            ));
        }
        line.extend_from_slice(chunk.get(..take).unwrap_or_default());
        reader.consume(take);
        if newline.is_some() {
            return to_string(line).map(Some);
        }
    }
}

fn to_string(bytes: Vec<u8>) -> io::Result<String> {
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "reply is not valid UTF-8"))
}

#[cfg(unix)]
#[allow(clippy::type_complexity)]
fn open(socket: &Path) -> io::Result<(Box<dyn BufRead>, Box<dyn Write>)> {
    let stream = std::os::unix::net::UnixStream::connect(socket)?;
    let writer = stream.try_clone()?;
    Ok((Box::new(BufReader::new(stream)), Box::new(writer)))
}

#[cfg(not(unix))]
#[allow(clippy::type_complexity)]
fn open(_socket: &Path) -> io::Result<(Box<dyn BufRead>, Box<dyn Write>)> {
    let _ = BufReader::new(io::empty());
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix sockets are not available on this platform",
    ))
}

/// Pulls the message out of an error reply, if that is what this is.
pub fn error_of(response: &Response) -> Option<(&'static str, &str)> {
    match response {
        Response::Error { kind, message } => Some((kind.as_str(), message.as_str())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use craft_a_server_core::ctl::ErrorKind;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A writer that hands its bytes back to the test.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn request_writes_a_line_and_reads_the_reply() -> TestResult {
        let reply = encode_line(&Reply {
            id: Some(1),
            response: Response::Ok,
        });
        let sink = Sink::default();
        let mut client = Client::from_parts(
            Box::new(Cursor::new(reply.into_bytes())),
            Box::new(sink.clone()),
        );
        let got = client.request(Command::Ping {})?;
        assert_eq!(got.response, Response::Ok);
        let sent = String::from_utf8(sink.0.lock().map_err(|_| "poisoned")?.clone())?;
        assert_eq!(sent, "{\"id\":1,\"cmd\":{\"type\":\"ping\"}}\n");
        Ok(())
    }

    #[test]
    fn closed_connection_is_an_io_error() {
        let mut client =
            Client::from_parts(Box::new(Cursor::new(Vec::new())), Box::new(Sink::default()));
        assert!(matches!(
            client.request(Command::Ping {}),
            Err(ClientError::Io(_))
        ));
    }

    #[test]
    fn garbage_is_a_protocol_error() {
        let mut client = Client::from_parts(
            Box::new(Cursor::new(b"hello\n".to_vec())),
            Box::new(Sink::default()),
        );
        assert!(matches!(
            client.request(Command::Ping {}),
            Err(ClientError::Protocol(_))
        ));
    }

    #[test]
    fn line_reader_is_bounded() -> TestResult {
        let mut r = Cursor::new(b"abc\ndef".to_vec());
        assert_eq!(read_line_bounded(&mut r, 10)?.as_deref(), Some("abc\n"));
        assert_eq!(read_line_bounded(&mut r, 10)?.as_deref(), Some("def"));
        assert_eq!(read_line_bounded(&mut r, 10)?, None);
        let mut long = Cursor::new(vec![b'a'; 100]);
        assert!(read_line_bounded(&mut long, 10).is_err());
        Ok(())
    }

    fn server(id: &str, name: &str) -> ServerInfo {
        ServerInfo {
            id: id.into(),
            name: name.into(),
            mc_version: "26.3".into(),
            state: "stopped".into(),
            reason: None,
            port: 25565,
            created_unix: 0,
        }
    }

    #[test]
    fn a_server_is_found_by_id_or_by_a_unique_name() -> TestResult {
        let all = [server("s1", "survival"), server("s2", "creative")];
        assert_eq!(pick_server(&all, "s2")?, Some("s2".into()));
        assert_eq!(pick_server(&all, "survival")?, Some("s1".into()));
        assert_eq!(pick_server(&all, "nope")?, None);
        Ok(())
    }

    #[test]
    fn an_id_wins_over_a_name_that_looks_like_it() -> TestResult {
        let all = [server("s1", "s2"), server("s2", "other")];
        assert_eq!(pick_server(&all, "s2")?, Some("s2".into()));
        Ok(())
    }

    #[test]
    fn a_name_shared_by_two_servers_is_refused() {
        let all = [server("s1", "world"), server("s2", "world")];
        assert!(matches!(
            pick_server(&all, "world"),
            Err(ClientError::Ambiguous(_))
        ));
    }

    #[test]
    fn a_typed_name_is_sent_to_the_daemon_as_the_id() -> TestResult {
        let list = encode_line(&Reply {
            id: Some(1),
            response: Response::Servers(vec![server("s1", "survival")]),
        });
        let ok = encode_line(&Reply {
            id: Some(2),
            response: Response::Ok,
        });
        let sink = Sink::default();
        let mut client = Client::from_parts(
            Box::new(Cursor::new(format!("{list}{ok}").into_bytes())),
            Box::new(sink.clone()),
        );
        client.request(Command::Start {
            id: "survival".into(),
        })?;
        let sent = String::from_utf8(sink.0.lock().map_err(|_| "poisoned")?.clone())?;
        let mut lines = sent.lines();
        assert_eq!(lines.next(), Some("{\"id\":1,\"cmd\":{\"type\":\"list\"}}"));
        assert_eq!(
            lines.next(),
            Some("{\"id\":2,\"cmd\":{\"type\":\"start\",\"id\":\"s1\"}}")
        );
        Ok(())
    }

    #[test]
    fn a_device_id_is_not_treated_as_a_server_name() -> TestResult {
        let ok = encode_line(&Reply {
            id: Some(1),
            response: Response::Ok,
        });
        let sink = Sink::default();
        let mut client = Client::from_parts(
            Box::new(Cursor::new(ok.into_bytes())),
            Box::new(sink.clone()),
        );
        client.request(Command::DevicesRevoke { id: "d1".into() })?;
        let sent = String::from_utf8(sink.0.lock().map_err(|_| "poisoned")?.clone())?;
        assert_eq!(sent.lines().count(), 1, "no list lookup for a device id");
        Ok(())
    }

    #[test]
    fn error_extraction() {
        let r = Response::Error {
            kind: ErrorKind::NotFound,
            message: "no such server".into(),
        };
        assert_eq!(error_of(&r), Some(("not_found", "no such server")));
        assert_eq!(error_of(&Response::Ok), None);
    }
}
