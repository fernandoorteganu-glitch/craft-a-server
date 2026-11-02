//! Server container specification, validation, and the one place where the
//! hardened `podman` argument lists are built.

use std::fmt;
use std::path::PathBuf;

use super::{LABEL_ID, MC_PORT, RuntimeError, container_name, pod_name};

/// Everything needed to run one server's pod and game container.
#[derive(Clone)]
pub struct ServerContainerSpec {
    /// Server id: `[a-z0-9-]{1,32}`, not starting with `-`.
    pub id: String,
    /// Minecraft version passed as `VERSION`: `[0-9A-Za-z._-]{1,32}`.
    pub mc_version: String,
    /// Image reference (pin by digest in releases).
    pub image: String,
    /// Absolute host path mounted at `/data`.
    pub data_dir: PathBuf,
    /// Host port published (on all interfaces) to the pod's 25565.
    pub host_port: u16,
    /// Container memory limit (swap is set equal).
    pub memory_limit_mib: u32,
    /// JVM heap (`MEMORY` env); must leave room below the limit for non-heap use.
    pub heap_mib: u32,
    pub cpus: f64,
    pub pids_limit: u32,
    /// Non-root uid:gid the game runs as (the service user, via `keep-id`).
    pub uid: u32,
    pub gid: u32,
    /// Seconds podman waits after SIGTERM before SIGKILL (`--stop-timeout`).
    pub stop_timeout_secs: u32,
    /// Must be true; the EULA is never accepted implicitly.
    pub eula_accepted: bool,
    /// RCON password: `[A-Za-z0-9]{16,128}`. Never put on a command line.
    pub rcon_password: String,
    /// Chunks the server sends around each player (`VIEW_DISTANCE`); `None`
    /// leaves the game's own default.
    pub view_distance: Option<u8>,
}

impl fmt::Debug for ServerContainerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerContainerSpec")
            .field("id", &self.id)
            .field("mc_version", &self.mc_version)
            .field("image", &self.image)
            .field("data_dir", &self.data_dir)
            .field("host_port", &self.host_port)
            .field("memory_limit_mib", &self.memory_limit_mib)
            .field("heap_mib", &self.heap_mib)
            .field("cpus", &self.cpus)
            .field("pids_limit", &self.pids_limit)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("stop_timeout_secs", &self.stop_timeout_secs)
            .field("eula_accepted", &self.eula_accepted)
            .field("view_distance", &self.view_distance)
            .field("rcon_password", &"<redacted>")
            .finish()
    }
}

fn invalid(msg: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidSpec(msg.into())
}

/// `[a-z0-9-]{1,32}`, not starting with `-`.
pub fn validate_id(id: &str) -> Result<(), RuntimeError> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "id must match [a-z0-9-]{1,32} and not start with '-'",
        ))
    }
}

fn validate_mc_version(v: &str) -> Result<(), RuntimeError> {
    let ok = !v.is_empty()
        && v.len() <= 32
        && !v.starts_with('-')
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(invalid("mc_version must match [0-9A-Za-z._-]{1,32}"))
    }
}

fn validate_image(image: &str) -> Result<(), RuntimeError> {
    let ok = !image.is_empty()
        && image.len() <= 255
        && !image.starts_with(['-', '/', ':', '@'])
        && image.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':' | b'@')
        });
    if ok {
        Ok(())
    } else {
        Err(invalid("image reference has unexpected characters"))
    }
}

/// Linux-style absolute path without NUL, newline, ':' (volume separator), ','
/// or `..` components. Checked textually so it behaves the same on every host OS.
fn data_dir_str(path: &std::path::Path) -> Result<&str, RuntimeError> {
    let s = path
        .to_str()
        .ok_or_else(|| invalid("data_dir is not valid UTF-8"))?;
    let ok = s.starts_with('/')
        && s.len() <= 4096
        && !s.chars().any(|c| c.is_control() || matches!(c, ':' | ','))
        && !s.split('/').any(|c| c == "..");
    if ok {
        Ok(s)
    } else {
        Err(invalid(
            "data_dir must be an absolute path without control characters, ':', ',' or '..'",
        ))
    }
}

fn validate_password(p: &str) -> Result<(), RuntimeError> {
    if (16..=128).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(invalid("rcon_password must be 16-128 ASCII alphanumerics"))
    }
}

impl ServerContainerSpec {
    /// Checks every field that reaches an argument or environment variable.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        validate_id(&self.id)?;
        validate_mc_version(&self.mc_version)?;
        validate_image(&self.image)?;
        data_dir_str(&self.data_dir)?;
        validate_password(&self.rcon_password)?;
        if self.host_port == 0 {
            return Err(invalid("host_port must not be 0"));
        }
        if self.heap_mib < 128 {
            return Err(invalid("heap_mib must be at least 128"));
        }
        if self.memory_limit_mib <= self.heap_mib {
            return Err(invalid("memory_limit_mib must exceed heap_mib"));
        }
        if !(self.cpus.is_finite() && self.cpus > 0.0 && self.cpus <= 256.0) {
            return Err(invalid("cpus must be in (0, 256]"));
        }
        if !(16..=65_535).contains(&self.pids_limit) {
            return Err(invalid("pids_limit must be in 16..=65535"));
        }
        if self.uid == 0 || self.gid == 0 {
            return Err(invalid("the game must not run as uid/gid 0"));
        }
        if self.stop_timeout_secs > 3600 {
            return Err(invalid("stop_timeout_secs must be <= 3600"));
        }
        if self
            .view_distance
            .is_some_and(|v| !crate::settings::valid_view_distance(v))
        {
            return Err(invalid("view_distance is outside the allowed range"));
        }
        Ok(())
    }
}

/// Arguments for `podman pod create`: one pod per server, pasta networking,
/// `keep-id` so data files belong to the service user, Minecraft port published.
pub fn build_pod_create_args(spec: &ServerContainerSpec) -> Result<Vec<String>, RuntimeError> {
    spec.validate()?;
    Ok(vec![
        "pod".into(),
        "create".into(),
        "--name".into(),
        pod_name(&spec.id),
        "--network=pasta".into(),
        "--userns=keep-id".into(),
        "-p".into(),
        format!("{}:{MC_PORT}", spec.host_port),
        "--label".into(),
        format!("{LABEL_ID}={}", spec.id),
    ])
}

/// Arguments for `podman run` of the game container. The single place where the
/// hardening profile is defined; covered by a full-list snapshot test.
///
/// The RCON password is NOT in the list: `-e RCON_PASSWORD` (no value) makes
/// podman copy it from its own environment, so the caller must set it on the
/// spawned `podman` process (see [`ServerContainerSpec::rcon_password`]). It
/// therefore never appears in `ps` output.
pub fn build_run_args(spec: &ServerContainerSpec) -> Result<Vec<String>, RuntimeError> {
    spec.validate()?;
    if !spec.eula_accepted {
        return Err(RuntimeError::EulaNotAccepted);
    }
    let data_dir = data_dir_str(&spec.data_dir)?;
    let mem = format!("{}m", spec.memory_limit_mib);

    let mut args: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_owned()));

    push(&["run", "--detach"]);
    push(&["--pod", &pod_name(&spec.id)]);
    push(&["--name", &container_name(&spec.id)]);
    push(&["--label", &format!("{LABEL_ID}={}", spec.id)]);
    push(&["--read-only"]);
    push(&["--tmpfs", "/tmp"]);
    push(&["--cap-drop=ALL"]);
    push(&["--security-opt", "no-new-privileges"]);
    push(&[&format!("--memory={mem}"), &format!("--memory-swap={mem}")]);
    push(&[&format!("--cpus={:.2}", spec.cpus)]);
    push(&[&format!("--pids-limit={}", spec.pids_limit)]);
    push(&["--user", &format!("{}:{}", spec.uid, spec.gid)]);
    push(&["-v", &format!("{data_dir}:/data:rw")]);
    push(&[&format!("--stop-timeout={}", spec.stop_timeout_secs)]);
    push(&["-e", "EULA=TRUE"]);
    push(&["-e", "TYPE=FABRIC"]);
    push(&["-e", &format!("VERSION={}", spec.mc_version)]);
    push(&["-e", &format!("MEMORY={}M", spec.heap_mib)]);
    push(&["-e", "ENABLE_RCON=true"]);
    push(&["-e", "RCON_PASSWORD"]);
    push(&["-e", "ONLINE_MODE=TRUE"]);
    push(&["-e", "ENABLE_WHITELIST=TRUE"]);
    push(&["-e", "ENFORCE_WHITELIST=TRUE"]);
    if let Some(v) = spec.view_distance {
        push(&["-e", &format!("VIEW_DISTANCE={v}")]);
    }
    push(&[&spec.image]);
    Ok(args)
}

#[cfg(test)]
pub(crate) fn test_spec() -> ServerContainerSpec {
    ServerContainerSpec {
        id: "survival-1".into(),
        mc_version: "1.21.4".into(),
        image: "docker.io/itzg/minecraft-server:java21".into(),
        data_dir: PathBuf::from("/var/lib/craft-a-server/servers/survival-1/data"),
        host_port: 25565,
        memory_limit_mib: 3072,
        heap_mib: 2048,
        cpus: 2.0,
        pids_limit: 512,
        uid: 1000,
        gid: 1000,
        stop_timeout_secs: 30,
        eula_accepted: true,
        rcon_password: "0123456789abcdef0123456789abcdef".into(),
        view_distance: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn run_args_snapshot() -> Result<(), RuntimeError> {
        let args = build_run_args(&test_spec())?;
        let expected = strs(&[
            "run",
            "--detach",
            "--pod",
            "craft-survival-1",
            "--name",
            "craft-survival-1-mc",
            "--label",
            "craft-a-server.id=survival-1",
            "--read-only",
            "--tmpfs",
            "/tmp",
            "--cap-drop=ALL",
            "--security-opt",
            "no-new-privileges",
            "--memory=3072m",
            "--memory-swap=3072m",
            "--cpus=2.00",
            "--pids-limit=512",
            "--user",
            "1000:1000",
            "-v",
            "/var/lib/craft-a-server/servers/survival-1/data:/data:rw",
            "--stop-timeout=30",
            "-e",
            "EULA=TRUE",
            "-e",
            "TYPE=FABRIC",
            "-e",
            "VERSION=1.21.4",
            "-e",
            "MEMORY=2048M",
            "-e",
            "ENABLE_RCON=true",
            "-e",
            "RCON_PASSWORD",
            "-e",
            "ONLINE_MODE=TRUE",
            "-e",
            "ENABLE_WHITELIST=TRUE",
            "-e",
            "ENFORCE_WHITELIST=TRUE",
            "docker.io/itzg/minecraft-server:java21",
        ]);
        assert_eq!(args, expected);
        Ok(())
    }

    #[test]
    fn view_distance_reaches_the_container_only_when_set_and_in_range() -> Result<(), RuntimeError>
    {
        let mut spec = test_spec();
        let none = build_run_args(&spec)?;
        assert!(!none.iter().any(|a| a.starts_with("VIEW_DISTANCE")));

        spec.view_distance = Some(8);
        let args = build_run_args(&spec)?;
        let at = args
            .iter()
            .position(|a| a == "VIEW_DISTANCE=8")
            .ok_or_else(|| invalid("VIEW_DISTANCE missing"))?;
        assert_eq!(args.get(at - 1).map(String::as_str), Some("-e"));
        assert_eq!(args.len(), none.len() + 2);

        for bad in [0, 1, 3, 17, 32, 255] {
            spec.view_distance = Some(bad);
            assert!(build_run_args(&spec).is_err(), "accepted {bad}");
        }
        Ok(())
    }

    #[test]
    fn pod_create_args_snapshot() -> Result<(), RuntimeError> {
        let args = build_pod_create_args(&test_spec())?;
        assert_eq!(
            args,
            strs(&[
                "pod",
                "create",
                "--name",
                "craft-survival-1",
                "--network=pasta",
                "--userns=keep-id",
                "-p",
                "25565:25565",
                "--label",
                "craft-a-server.id=survival-1",
            ])
        );
        Ok(())
    }

    #[test]
    fn secret_and_dangerous_flags_never_appear() -> Result<(), RuntimeError> {
        let spec = test_spec();
        let joined = build_run_args(&spec)?.join(" ");
        assert!(!joined.contains(&spec.rcon_password));
        for bad in [
            "--privileged",
            "NET_ADMIN",
            "NET_RAW",
            "--network=host",
            "--net=host",
            "podman.sock",
            "docker.sock",
            "--cap-add",
            "--pid=host",
        ] {
            assert!(!joined.contains(bad), "{bad} must not appear");
        }
        // Every mount is the data dir.
        let mounts: Vec<_> = build_run_args(&spec)?
            .windows(2)
            .filter(|w| w.first().map(String::as_str) == Some("-v"))
            .filter_map(|w| w.get(1).cloned())
            .collect();
        assert_eq!(mounts.len(), 1);
        Ok(())
    }

    #[test]
    fn debug_redacts_password() {
        let dbg = format!("{:?}", test_spec());
        assert!(!dbg.contains("0123456789abcdef"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn eula_must_be_accepted() {
        let mut spec = test_spec();
        spec.eula_accepted = false;
        assert!(matches!(
            build_run_args(&spec),
            Err(RuntimeError::EulaNotAccepted)
        ));
        // The pod can still be described; only the game container needs the EULA.
        assert!(build_pod_create_args(&spec).is_ok());
    }

    fn rejects(f: impl FnOnce(&mut ServerContainerSpec)) -> bool {
        let mut spec = test_spec();
        f(&mut spec);
        spec.validate().is_err() && build_run_args(&spec).is_err()
    }

    #[test]
    fn validation_rejects_hostile_values() {
        assert!(rejects(|s| s.id = String::new()));
        assert!(rejects(|s| s.id = "UPPER".into()));
        assert!(rejects(|s| s.id = "a b".into()));
        assert!(rejects(|s| s.id = "-rf".into()));
        assert!(rejects(|s| s.id = "a".repeat(33)));
        assert!(rejects(|s| s.id = "a/../b".into()));
        assert!(rejects(|s| s.mc_version = "1.21;rm".into()));
        assert!(rejects(|s| s.mc_version = "a\nb".into()));
        assert!(rejects(|s| s.mc_version = String::new()));
        assert!(rejects(|s| s.image = "--privileged".into()));
        assert!(rejects(|s| s.image = "img name".into()));
        assert!(rejects(|s| s.image = String::new()));
        assert!(rejects(|s| s.data_dir = PathBuf::from("relative/dir")));
        assert!(rejects(|s| s.data_dir = PathBuf::from("/a\nb")));
        assert!(rejects(|s| s.data_dir = PathBuf::from("/a\0b")));
        assert!(rejects(|s| s.data_dir = PathBuf::from("/a:/etc:rw")));
        assert!(rejects(|s| s.data_dir = PathBuf::from("/a/../etc")));
        assert!(rejects(|s| s.host_port = 0));
        assert!(rejects(|s| s.heap_mib = 64));
        assert!(rejects(|s| s.memory_limit_mib = s.heap_mib));
        assert!(rejects(|s| s.cpus = f64::NAN));
        assert!(rejects(|s| s.cpus = 0.0));
        assert!(rejects(|s| s.pids_limit = 1));
        assert!(rejects(|s| s.uid = 0));
        assert!(rejects(|s| s.gid = 0));
        assert!(rejects(|s| s.rcon_password = "short".into()));
        assert!(rejects(|s| s.rcon_password = "a".repeat(15) + "\n"));
    }

    #[test]
    fn valid_spec_is_accepted() {
        assert!(test_spec().validate().is_ok());
    }
}
