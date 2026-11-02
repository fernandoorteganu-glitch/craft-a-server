//! Reading the playit "agent run data" answer.
//!
//! `POST https://api.playit.gg/agents/rundata` returns the tunnels of the
//! agent. The API is undocumented, so everything here is defensive: unknown
//! fields are ignored, missing ones mean "not usable", and the only thing
//! that can reach a screen is an address built by
//! [`craft_a_server_core::tunnel::validate_public_address`] from a
//! hostname-and-port pair that passed its checks.
//!
//! The expected shape (from the agent's source, not yet seen from a live
//! account) is `{"status":"success","data":{"tunnels":[{"tunnel_type":
//! "minecraft-java","assigned_domain":"x.craft.playit.gg","port":{"from":
//! 12345,"to":12345},"local_ip":"127.0.0.1","local_port":25565,"disabled":
//! null}]}}`.

use std::net::IpAddr;

use craft_a_server_core::tunnel::validate_public_address;
use serde_json::Value;

/// Where to ask. Undocumented endpoint; the path is the agent's own.
pub const RUNDATA_URL: &str = "https://api.playit.gg/agents/rundata";

/// Largest answer read.
pub const MAX_RUNDATA_BYTES: u64 = 512 * 1024;

/// What the answer says about our Minecraft tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rundata {
    /// A usable tunnel with this (validated) public address.
    Address(String),
    /// The account has no Minecraft Java tunnel pointing at our port.
    NoTunnel,
    /// A matching tunnel exists but forwards somewhere other than this
    /// machine's loopback; it is not shown or trusted.
    UnsafeOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RundataError {
    #[error("playit.gg answered in a format this version does not understand")]
    Unreadable,
    #[error("playit.gg refused the request")]
    Refused,
}

fn is_loopback(text: &str) -> bool {
    text.eq_ignore_ascii_case("localhost")
        || text.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn as_port(v: Option<&Value>) -> Option<u32> {
    v.and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Reads the answer body for the tunnel that forwards to `mc_port`.
pub fn parse_rundata(body: &[u8], mc_port: u16) -> Result<Rundata, RundataError> {
    let root: Value = serde_json::from_slice(body).map_err(|_| RundataError::Unreadable)?;
    // `{"status":"success","data":{...}}`; tolerate a bare data object.
    let data = match root.get("status").and_then(Value::as_str) {
        Some("success") => root.get("data").ok_or(RundataError::Unreadable)?,
        Some(_) => return Err(RundataError::Refused),
        None => &root,
    };
    let tunnels = data
        .get("tunnels")
        .and_then(Value::as_array)
        .ok_or(RundataError::Unreadable)?;

    let mut unsafe_origin = false;
    for tunnel in tunnels {
        if tunnel
            .get("disabled")
            .is_some_and(|d| !d.is_null() && d != &Value::Bool(false))
        {
            continue;
        }
        let local_port = as_port(tunnel.get("local_port"));
        let is_java = tunnel.get("tunnel_type").and_then(Value::as_str) == Some("minecraft-java");
        let ours = match local_port {
            Some(p) => p == u32::from(mc_port),
            None => is_java,
        };
        if !ours {
            continue;
        }
        if let Some(ip) = tunnel.get("local_ip").and_then(Value::as_str)
            && !is_loopback(ip)
        {
            unsafe_origin = true;
            continue;
        }
        let host = ["custom_domain", "assigned_domain"].iter().find_map(|key| {
            tunnel
                .get(key)
                .and_then(Value::as_str)
                .filter(|h| !h.trim().is_empty())
        });
        let port = as_port(tunnel.get("port").and_then(|p| p.get("from")));
        if let (Some(host), Some(port)) = (host, port)
            && let Some(address) = validate_public_address(host, port)
        {
            return Ok(Rundata::Address(address));
        }
    }
    Ok(if unsafe_origin {
        Rundata::UnsafeOrigin
    } else {
        Rundata::NoTunnel
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(tunnels: &str) -> Vec<u8> {
        format!(r#"{{"status":"success","data":{{"agent_id":"x","tunnels":[{tunnels}]}}}}"#)
            .into_bytes()
    }

    const GOOD: &str = r#"{"id":"t1","tunnel_type":"minecraft-java","assigned_domain":"mine.craft.playit.gg","port":{"from":12345,"to":12345},"local_ip":"127.0.0.1","local_port":25565,"disabled":null}"#;

    #[test]
    fn picks_the_minecraft_tunnel_for_our_port() {
        assert_eq!(
            parse_rundata(&wrap(GOOD), 25565),
            Ok(Rundata::Address("mine.craft.playit.gg:12345".into()))
        );
    }

    #[test]
    fn default_port_is_left_out_and_custom_domains_win() {
        let t = r#"{"tunnel_type":"minecraft-java","assigned_domain":"a.playit.gg","custom_domain":"play.example.org","port":{"from":25565},"local_ip":"127.0.0.1","local_port":25565}"#;
        assert_eq!(
            parse_rundata(&wrap(t), 25565),
            Ok(Rundata::Address("play.example.org".into()))
        );
    }

    #[test]
    fn other_tunnels_and_disabled_ones_are_skipped() {
        let other = r#"{"tunnel_type":"minecraft-bedrock","assigned_domain":"b.playit.gg","port":{"from":1},"local_port":19132}"#;
        let disabled = r#"{"tunnel_type":"minecraft-java","assigned_domain":"c.playit.gg","port":{"from":2},"local_port":25565,"disabled":"UserDisabled"}"#;
        assert_eq!(
            parse_rundata(&wrap(&format!("{other},{disabled}")), 25565),
            Ok(Rundata::NoTunnel)
        );
        assert_eq!(
            parse_rundata(&wrap(&format!("{other},{disabled},{GOOD}")), 25565),
            Ok(Rundata::Address("mine.craft.playit.gg:12345".into()))
        );
        // The wrong local port is somebody else's tunnel.
        assert_eq!(parse_rundata(&wrap(GOOD), 25566), Ok(Rundata::NoTunnel));
    }

    #[test]
    fn a_tunnel_that_forwards_off_this_machine_is_not_trusted() {
        let lan = r#"{"tunnel_type":"minecraft-java","assigned_domain":"d.playit.gg","port":{"from":3},"local_ip":"192.168.1.20","local_port":25565}"#;
        assert_eq!(parse_rundata(&wrap(lan), 25565), Ok(Rundata::UnsafeOrigin));
        // A good tunnel next to it wins.
        assert_eq!(
            parse_rundata(&wrap(&format!("{lan},{GOOD}")), 25565),
            Ok(Rundata::Address("mine.craft.playit.gg:12345".into()))
        );
    }

    #[test]
    fn hostile_addresses_never_come_out() {
        for (host, port) in [
            ("<script>alert(1)</script>", "12345"),
            ("evil host.playit.gg", "12345"),
            ("MINE.playit.gg", "12345"),
            ("1.2.3.4", "12345"),
            ("mine.playit.gg\\n", "12345"),
            ("mine.playit.gg", "0"),
            ("mine.playit.gg", "70000"),
            ("mine.playit.gg", "-1"),
            ("mine.playit.gg", "\"12\""),
            ("javascript:alert(1)//x.gg", "12345"),
        ] {
            let t = format!(
                r#"{{"tunnel_type":"minecraft-java","assigned_domain":"{host}","port":{{"from":{port}}},"local_ip":"127.0.0.1","local_port":25565}}"#
            );
            assert_eq!(
                parse_rundata(&wrap(&t), 25565),
                Ok(Rundata::NoTunnel),
                "{host}:{port}"
            );
        }
    }

    #[test]
    fn garbage_and_refusals_are_errors() {
        assert_eq!(parse_rundata(b"", 25565), Err(RundataError::Unreadable));
        assert_eq!(
            parse_rundata(b"<html>", 25565),
            Err(RundataError::Unreadable)
        );
        assert_eq!(parse_rundata(b"[]", 25565), Err(RundataError::Unreadable));
        assert_eq!(
            parse_rundata(br#"{"status":"success","data":{}}"#, 25565),
            Err(RundataError::Unreadable)
        );
        assert_eq!(
            parse_rundata(br#"{"status":"error","data":"Unauthorized"}"#, 25565),
            Err(RundataError::Refused)
        );
        assert_eq!(
            parse_rundata(br#"{"status":"fail","data":{}}"#, 25565),
            Err(RundataError::Refused)
        );
        assert_eq!(
            parse_rundata(br#"{"tunnels":[]}"#, 25565),
            Ok(Rundata::NoTunnel)
        );
    }

    #[test]
    fn a_tunnel_without_a_port_is_unusable_not_a_crash() {
        let t = r#"{"tunnel_type":"minecraft-java","assigned_domain":"mine.craft.playit.gg","local_port":25565}"#;
        assert_eq!(parse_rundata(&wrap(t), 25565), Ok(Rundata::NoTunnel));
        let weird = r#"{"tunnel_type":5,"assigned_domain":7,"port":"x","local_port":"y"}"#;
        assert_eq!(parse_rundata(&wrap(weird), 25565), Ok(Rundata::NoTunnel));
    }

    #[test]
    fn loopback_detection() {
        for ok in ["127.0.0.1", "127.1.2.3", "::1", "localhost", "LocalHost"] {
            assert!(is_loopback(ok), "{ok}");
        }
        for bad in ["0.0.0.0", "192.168.1.1", "10.0.0.1", "example.com", ""] {
            assert!(!is_loopback(bad), "{bad}");
        }
    }
}
