//! Network-level gates, as pure functions: who may connect, which `Host`
//! values are accepted (DNS rebinding defence) and whether a state-changing
//! request really comes from the panel's own pages (`Origin` / `Referer`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn v4_allowed(ip: Ipv4Addr) -> bool {
    // is_private = 10/8, 172.16/12, 192.168/16. 100.64/10 (CGNAT) is NOT
    // allowed: an address there is not on the home LAN.
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn v6_allowed(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
}

/// May a TCP peer at this address talk to the panel at all?
///
/// Loopback, RFC 1918, IPv4 link-local, IPv6 `::1`, ULA `fc00::/7` and
/// link-local `fe80::/10`. Everything else, notably public IPv4 and IPv6
/// global unicast (a LAN with IPv6 and a permissive router could otherwise
/// expose the panel to the internet), is refused. IPv4-mapped IPv6 addresses
/// (`::ffff:a.b.c.d`, how a dual-stack socket reports IPv4 peers) are judged as
/// the IPv4 address they carry.
pub fn peer_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4_allowed(v4),
            None => v6_allowed(v6),
        },
    }
}

/// The peer address with IPv4-mapped IPv6 turned back into IPv4, so rate-limit
/// keys do not depend on the socket family.
pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|p| p != 0)
}

/// Is this `Host` header value one the panel answers to?
///
/// Accepted: the configured panel hostname, `localhost`, or an IP literal that
/// is itself loopback/private/link-local/ULA, each optionally followed by
/// `:port` (IPv6 literals in brackets). Names compare ASCII case-insensitively
/// (DNS is case-insensitive). A trailing dot, userinfo, paths, whitespace,
/// non-ASCII and anything else is refused, so `craft-a-server.local.evil.com`,
/// `craft-a-server.local@evil.com` and friends cannot pass.
pub fn host_allowed(host: &str, panel_hostname: &str) -> bool {
    if host.is_empty() || host.len() > 255 || !host.is_ascii() {
        return false;
    }
    if host
        .bytes()
        .any(|b| b.is_ascii_control() || matches!(b, b' ' | b'@' | b'/' | b'\\' | b'?' | b'#'))
    {
        return false;
    }
    if let Some(rest) = host.strip_prefix('[') {
        let Some((inner, after)) = rest.split_once(']') else {
            return false;
        };
        let port_ok = match after {
            "" => true,
            _ => after.strip_prefix(':').is_some_and(valid_port),
        };
        // `parse` rejects zone ids (`%eth0`) and anything but a plain address.
        return port_ok
            && inner
                .parse::<Ipv6Addr>()
                .is_ok_and(|ip| peer_allowed(IpAddr::V6(ip)));
    }
    let (name, port_ok) = match host.split_once(':') {
        None => (host, true),
        Some((name, port)) => (name, valid_port(port)),
    };
    if !port_ok || name.is_empty() {
        return false;
    }
    if name.eq_ignore_ascii_case("localhost") || name.eq_ignore_ascii_case(panel_hostname) {
        return true;
    }
    name.parse::<Ipv4Addr>()
        .is_ok_and(|ip| peer_allowed(IpAddr::V4(ip)))
}

/// Does a state-changing request come from the panel itself?
///
/// `host` must already have passed [`host_allowed`]. If `Origin` is present it
/// must be exactly `http://<host>` (`null` and anything else fail, with no
/// fallback to `Referer`). Without `Origin`, `Referer` must point at
/// `http://<host>/...`. With neither, the request is refused: the panel's own
/// forms and htmx requests always send at least one of them.
pub fn origin_ok(origin: Option<&str>, referer: Option<&str>, host: &str) -> bool {
    if let Some(origin) = origin {
        return origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("HTTP://"))
            .is_some_and(|h| h.eq_ignore_ascii_case(host));
    }
    if let Some(referer) = referer {
        let Some(rest) = referer
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("http://"))
            .and_then(|_| referer.get(7..))
        else {
            return false;
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        return authority.eq_ignore_ascii_case(host);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }

    #[test]
    fn peer_filter_table() {
        let allowed = [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "169.254.1.1",
            "::1",
            "fc00::1",
            "fd12:3456:789a::1",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe80::1",
            "febf::1",
            "::ffff:192.168.1.5",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
        ];
        let denied = [
            "8.8.8.8",
            "1.1.1.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "11.0.0.1",
            "169.253.0.1",
            "2001:db8::1",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
            "::",
            "ff02::1",
            "fec0::1",
            "fb00::1",
            "fe00::1",
            "::192.168.1.5",
            "::ffff:8.8.8.8",
            "::ffff:100.64.0.1",
            "64:ff9b::c0a8:0105",
            "2002:c0a8:0105::1",
        ];
        for a in allowed {
            assert!(peer_allowed(ip(a)), "{a} should be allowed");
        }
        for d in denied {
            assert!(!peer_allowed(ip(d)), "{d} should be denied");
        }
    }

    #[test]
    fn unmap_turns_mapped_v6_into_v4() {
        assert_eq!(unmap(ip("::ffff:10.1.2.3")), ip("10.1.2.3"));
        assert_eq!(unmap(ip("fe80::1")), ip("fe80::1"));
        assert_eq!(unmap(ip("10.1.2.3")), ip("10.1.2.3"));
    }

    #[test]
    fn host_matrix() {
        let me = "craft-a-server.local";
        let good = [
            "craft-a-server.local",
            "craft-a-server.local:8080",
            "CRAFT-A-SERVER.LOCAL",
            "Craft-A-Server.Local:80",
            "localhost",
            "localhost:8080",
            "LOCALHOST",
            "127.0.0.1",
            "127.0.0.1:18080",
            "192.168.1.50:8080",
            "10.0.0.2",
            "172.16.5.5:80",
            "169.254.10.10",
            "[::1]",
            "[::1]:8080",
            "[fd00::1]:8080",
            "[fe80::1]",
            "[::ffff:192.168.1.5]",
        ];
        let bad = [
            "",
            "evil.com",
            "evil.com:8080",
            "craft-a-server.local.evil.com",
            "evil.craft-a-server.local",
            "craft-a-server.local@evil.com",
            "evil.com@craft-a-server.local",
            "user:pw@craft-a-server.local",
            "craft-a-server.local.",
            "craft-a-server.local.:8080",
            "craft-a-server.local:",
            "craft-a-server.local:0",
            "craft-a-server.local:99999",
            "craft-a-server.local:80a",
            "craft-a-server.local:80:80",
            "craft-a-server.local/evil",
            "craft-a-server.local\\evil",
            "craft-a-server.local evil",
            "craft-a-server.local\t",
            "craft-a-server.localé",
            "foo.localhost",
            "localhost.",
            "8.8.8.8",
            "8.8.8.8:8080",
            "100.64.0.1",
            "0.0.0.0",
            "192.168.1",
            "192.168.1.256",
            "192.168.001.001",
            "0x7f.0.0.1",
            "2130706433",
            "::1",
            "[::1",
            "::1]",
            "[::1]x",
            "[::1]:",
            "[::1]:99999",
            "[2001:db8::1]",
            "[2606:4700:4700::1111]:80",
            "[fe80::1%eth0]",
            "[::ffff:8.8.8.8]",
            "[]",
        ];
        for h in good {
            assert!(host_allowed(h, me), "{h:?} should be accepted");
        }
        for h in bad {
            assert!(!host_allowed(h, me), "{h:?} should be rejected");
        }
        // The configured name is what counts, not the default.
        assert!(host_allowed("mc.home:8080", "mc.home"));
        assert!(!host_allowed("craft-a-server.local", "mc.home"));
        assert!(!host_allowed(&"a".repeat(300), me));
    }

    #[test]
    fn origin_matrix() {
        let host = "craft-a-server.local:8080";
        let origin_cases = [
            (Some("http://craft-a-server.local:8080"), true),
            (Some("HTTP://CRAFT-A-SERVER.LOCAL:8080"), true),
            (Some("null"), false),
            (Some(""), false),
            (Some("https://craft-a-server.local:8080"), false),
            (Some("http://craft-a-server.local"), false),
            (Some("http://craft-a-server.local:8081"), false),
            (Some("http://evil.com"), false),
            (Some("http://craft-a-server.local:8080.evil.com"), false),
            (Some("http://craft-a-server.local:8080/"), false),
            (Some("http://craft-a-server.local:8080@evil.com"), false),
            (Some("file://"), false),
        ];
        for (origin, want) in origin_cases {
            assert_eq!(origin_ok(origin, None, host), want, "Origin {origin:?}");
            // A present Origin is final: a good Referer cannot rescue it.
            if !want {
                assert!(
                    !origin_ok(origin, Some("http://craft-a-server.local:8080/pair"), host),
                    "Origin {origin:?} with good Referer"
                );
            }
        }
        let referer_cases = [
            (Some("http://craft-a-server.local:8080/pair"), true),
            (Some("http://craft-a-server.local:8080"), true),
            (Some("http://craft-a-server.local:8080?x=1"), true),
            (Some("http://craft-a-server.local:8080#frag"), true),
            (Some("http://evil.com/craft-a-server.local:8080"), false),
            (Some("http://craft-a-server.local:8080@evil.com/"), false),
            (Some("http://evil.com@craft-a-server.local:8080/"), false),
            (Some("https://craft-a-server.local:8080/"), false),
            (Some("craft-a-server.local:8080/"), false),
            (Some(""), false),
            (Some("http:/"), false),
            (None, false),
        ];
        for (referer, want) in referer_cases {
            assert_eq!(origin_ok(None, referer, host), want, "Referer {referer:?}");
        }
        assert!(!origin_ok(None, None, host));
    }
}
