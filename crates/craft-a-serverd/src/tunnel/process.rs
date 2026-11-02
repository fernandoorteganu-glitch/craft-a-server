//! Running the agent as a child of the daemon.
//!
//! * Executables are started by absolute path with an argument vector and a
//!   scrubbed environment; nothing goes through a shell.
//! * The secret is never an argument or an environment variable: only the
//!   *path* of the secret file is passed (`--secret-path`).
//! * Output is read in bounded pieces; whatever is kept is passed through
//!   [`redact`] first, and the claim program's output is only searched for the
//!   claim link, never logged.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_a_server_core::tunnel::{CLAIM_URL_PREFIX, parse_claim_url};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use super::install::AgentPaths;

/// Longest line kept from the agent's output, in bytes.
pub const MAX_LINE_BYTES: usize = 512;
/// Lines of the agent's stderr kept for error messages.
pub const TAIL_LINES: usize = 20;

/// Where the tunnel keeps its files; everything under one private directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelDirs {
    pub root: PathBuf,
    /// Downloaded executables (one directory per version).
    pub bin: PathBuf,
    /// The agent's secret key (written by the agent, mode 0600).
    pub secret_path: PathBuf,
    /// The agent's IPC socket.
    pub socket_path: PathBuf,
    /// Process id of the running agent, to clean up after a daemon crash.
    pub pid_path: PathBuf,
    /// `HOME` of the agent, so it writes nothing outside our directory.
    pub home: PathBuf,
}

impl TunnelDirs {
    pub fn new(root: &Path) -> TunnelDirs {
        TunnelDirs {
            root: root.to_path_buf(),
            bin: root.join("bin"),
            secret_path: root.join("playit.toml"),
            socket_path: root.join("playitd.sock"),
            pid_path: root.join("agent.pid"),
            home: root.join("home"),
        }
    }
}

/// Replaces every run of 24 or more hex digits (what a secret key looks
/// like) with `[hidden]`. Applied to anything that may be stored or logged.
pub fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 24 {
            out.push_str("[hidden]");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in line.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// A printable, bounded, redacted version of an output line.
pub fn clean_line(line: &str) -> String {
    let printable: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    redact(printable.trim())
}

/// Finds `https://playit.gg/claim/<code>` in a line of the claim program's
/// output and returns the validated `(code, url)`.
pub fn extract_claim_url(line: &str) -> Option<(String, String)> {
    let start = line.find(CLAIM_URL_PREFIX)?;
    let tail = line.get(start..)?;
    let end = tail
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == '>')
        .unwrap_or(tail.len());
    parse_claim_url(tail.get(..end)?)
}

/// Splits a byte stream into lines without ever holding more than
/// [`MAX_LINE_BYTES`] of one line: the rest of an over-long line is dropped.
#[derive(Debug, Default)]
pub struct LineSplitter {
    current: Vec<u8>,
    skipping: bool,
}

impl LineSplitter {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                lines.push(String::from_utf8_lossy(&self.current).into_owned());
                self.current.clear();
                self.skipping = false;
            } else if self.current.len() < MAX_LINE_BYTES {
                self.current.push(b);
            } else {
                self.skipping = true;
            }
        }
        lines
    }

    /// The unfinished last line, if any (at end of stream).
    pub fn finish(&mut self) -> Option<String> {
        if self.current.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.current).into_owned();
        self.current.clear();
        Some(line)
    }

    #[cfg(test)]
    fn truncated(&self) -> bool {
        self.skipping
    }
}

/// Reads `reader` to the end, calling `on_line` for every line.
pub async fn pump_lines<R, F>(mut reader: R, mut on_line: F)
where
    R: AsyncRead + Unpin,
    F: FnMut(String),
{
    let mut splitter = LineSplitter::default();
    let mut buf = [0_u8; 2048];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for line in splitter.push(buf.get(..n).unwrap_or_default()) {
                    on_line(line);
                }
            }
        }
    }
    if let Some(line) = splitter.finish() {
        on_line(line);
    }
}

/// The last few cleaned lines of an output stream.
#[derive(Debug, Default, Clone)]
pub struct Tail(Arc<Mutex<VecDeque<String>>>);

impl Tail {
    pub fn push(&self, line: &str) {
        let cleaned = clean_line(line);
        if cleaned.is_empty() {
            return;
        }
        let mut lines = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if lines.len() >= TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(cleaned);
    }

    pub fn last(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .back()
            .cloned()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

fn base_command(program: &Path, dirs: &TunnelDirs) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .env("HOME", &dirs.home)
        .env("XDG_CONFIG_HOME", dirs.home.join(".config"))
        .env("XDG_DATA_HOME", dirs.home.join(".local/share"))
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .current_dir(&dirs.root)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// Starts the agent (`playitd`). Output: stdout discarded, stderr piped.
pub fn spawn_daemon(paths: &AgentPaths, dirs: &TunnelDirs) -> io::Result<Child> {
    super::install::ensure_private_dir(&dirs.home)?;
    let mut cmd = base_command(&paths.daemon, dirs);
    cmd.arg("--secret-path")
        .arg(&dirs.secret_path)
        .arg("--socket-path")
        .arg(&dirs.socket_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.spawn()
}

/// Starts the claim program: `playit-cli --socket-path S setup`. Output:
/// stdout piped (the claim link is on it), stderr discarded.
pub fn spawn_claim_cli(paths: &AgentPaths, dirs: &TunnelDirs) -> io::Result<Child> {
    super::install::ensure_private_dir(&dirs.home)?;
    let mut cmd = base_command(&paths.cli, dirs);
    cmd.arg("--socket-path")
        .arg(&dirs.socket_path)
        .arg("setup")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.spawn()
}

/// Asks a child to stop (SIGTERM on Unix), then kills it after `grace`.
pub async fn terminate(child: &mut Child, grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok())
        && let Some(pid) = rustix::process::Pid::from_raw(pid)
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
}

/// Records the agent's process id so a crashed daemon's orphan can be
/// stopped at the next start.
pub fn write_pid(path: &Path, pid: u32) {
    let _ = std::fs::write(path, format!("{pid}\n"));
}

pub fn remove_pid(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Stops an agent left running by a previous daemon: the process in the pid
/// file is signalled only if `/proc/<pid>/exe` is exactly `expected_exe`.
#[cfg(unix)]
pub async fn reap_stale(pid_path: &Path, expected_exe: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(pid_path) else {
        return false;
    };
    let _ = std::fs::remove_file(pid_path);
    let Some(pid) = text
        .trim()
        .parse::<i32>()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    let exe = std::fs::read_link(format!("/proc/{}/exe", pid.as_raw_nonzero()));
    if exe.ok().as_deref() != Some(expected_exe) {
        return false;
    }
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if rustix::process::test_kill_process(pid).is_err() {
            return true;
        }
    }
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    true
}

#[cfg(not(unix))]
pub async fn reap_stale(_pid_path: &Path, _expected_exe: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn long_hex_runs_are_hidden() {
        assert_eq!(redact("nothing here"), "nothing here");
        assert_eq!(
            redact(&format!("secret_key = \"{SECRET}\" done")),
            "secret_key = \"[hidden]\" done"
        );
        assert_eq!(redact(&format!("{SECRET}{SECRET}")), "[hidden]");
        // Short hex (a claim code, a pid) is kept.
        assert_eq!(
            redact("code a1b2c3d4e5 pid 1234"),
            "code a1b2c3d4e5 pid 1234"
        );
        assert_eq!(redact(""), "");
        // 23 digits stay, 24 go.
        assert_eq!(redact(&"a".repeat(23)), "a".repeat(23));
        assert_eq!(redact(&"a".repeat(24)), "[hidden]");
        // Multi-byte text is preserved.
        assert_eq!(redact("h\u{e9}llo"), "h\u{e9}llo");
    }

    #[test]
    fn clean_line_is_printable_and_redacted() {
        let line = clean_line(&format!("\u{1b}[31merror\u{1b}[0m key={SECRET}\r"));
        assert!(!line.contains('\u{1b}') && !line.contains(SECRET), "{line}");
        assert!(line.contains("error"));
    }

    #[test]
    fn finds_the_claim_link_in_cli_output() {
        let url = "https://playit.gg/claim/a1b2c3d4e5";
        for line in [
            url.to_owned(),
            format!("  {url}  "),
            format!("Open this link: {url}"),
            format!("<{url}>"),
            format!("(see {url})"),
        ] {
            assert_eq!(
                extract_claim_url(&line),
                Some(("a1b2c3d4e5".to_owned(), url.to_owned())),
                "{line}"
            );
        }
        for line in [
            "Open this link to finish setting up playit:",
            "https://evil.example/claim/a1b2c3d4e5",
            "https://playit.gg/claim/",
            "https://playit.gg/claim/<script>",
            "http://playit.gg/claim/a1b2c3d4e5 but not https",
            "",
        ] {
            assert_eq!(extract_claim_url(line), None, "{line}");
        }
        // A link followed by junk keeps only the validated part.
        assert_eq!(
            extract_claim_url("https://playit.gg/claim/a1b2c3d4e5?x=1"),
            None
        );
    }

    #[test]
    fn splitter_handles_partial_long_and_binary_input() {
        let mut s = LineSplitter::default();
        assert_eq!(s.push(b"hel"), Vec::<String>::new());
        assert_eq!(s.push(b"lo\nwor"), ["hello"]);
        assert_eq!(s.push(b"ld\n\n"), ["world", ""]);
        assert_eq!(s.finish(), None);
        s.push(b"tail");
        assert_eq!(s.finish().as_deref(), Some("tail"));

        let long = vec![b'x'; MAX_LINE_BYTES * 10];
        let mut lines = s.push(&long);
        assert!(lines.is_empty());
        assert!(s.truncated());
        lines = s.push(b"\nnext\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.first().map(String::len), Some(MAX_LINE_BYTES));
        assert_eq!(lines.get(1).map(String::as_str), Some("next"));
        assert!(!s.truncated());

        // Invalid UTF-8 is replaced, not an error.
        let out = s.push(&[0xff, 0xfe, b'\n']);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn tail_keeps_the_last_lines_cleaned() {
        let t = Tail::default();
        for i in 0..(TAIL_LINES + 5) {
            t.push(&format!("line {i}"));
        }
        t.push(&format!("key {SECRET}"));
        t.push("   ");
        assert_eq!(t.len(), TAIL_LINES);
        assert_eq!(t.last().as_deref(), Some("key [hidden]"));
    }

    #[test]
    fn dirs_are_all_under_the_root() {
        let d = TunnelDirs::new(Path::new("/var/lib/craft-a-server/tunnel"));
        for p in [&d.bin, &d.secret_path, &d.socket_path, &d.pid_path, &d.home] {
            assert!(p.starts_with(&d.root), "{p:?}");
        }
    }

    #[tokio::test]
    async fn pump_reads_lines_from_a_stream() {
        let data: &[u8] = b"one\ntwo\nthree";
        let mut got = Vec::new();
        pump_lines(data, |l| got.push(l)).await;
        assert_eq!(got, ["one", "two", "three"]);
    }
}
