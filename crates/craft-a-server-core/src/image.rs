//! Which Java a Minecraft version needs, and the matching server image tag.
//!
//! The itzg image publishes `javaNN` tags; `latest` always moves to the newest
//! Java, which breaks older worlds. Mojang's requirements: 1.16 and older run
//! on Java 8, 1.17-1.20.4 on 17, 1.20.5-1.21.x on 21, and the year-based
//! `26.x` releases on 25 (verified: 26.3 starts on the java25 image).

/// Java major version a Minecraft release needs.
pub fn java_major_for(mc_version: &str) -> Option<u32> {
    let mut parts = mc_version.split('.');
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        // Legacy scheme: 1.<minor>[.<patch>]
        let minor: u32 = parts.next()?.parse().ok()?;
        let patch: u32 = match parts.next() {
            Some(p) => p.parse().ok()?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        return Some(match minor {
            0..=16 => 8,
            17..=19 => 17,
            20 if patch >= 5 => 21,
            20 => 17,
            _ => 21,
        });
    }
    // Year-based scheme: <yy>.<n>[.<patch>]; the digits must all be numbers.
    let rest_ok = parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !rest_ok {
        return None;
    }
    Some(if first >= 26 { 25 } else { 21 })
}

/// The image reference to run for `mc_version`.
///
/// Only the moving `:latest` tag (or a missing tag) is rewritten to the
/// matching `:javaNN`. An image the operator pinned (another tag, or a
/// digest) is used exactly as configured, and so is an unknown version.
pub fn resolve_image(configured: &str, mc_version: &str) -> String {
    let Some(java) = java_major_for(mc_version) else {
        return configured.to_owned();
    };
    if configured.contains('@') {
        return configured.to_owned();
    }
    let (repo, tag) = match configured.rsplit_once(':') {
        // A ':' inside the registry host (host:5000/repo) is not a tag.
        Some((repo, tag)) if !tag.contains('/') => (repo, Some(tag)),
        _ => (configured, None),
    };
    match tag {
        None | Some("latest") => format!("{repo}:java{java}"),
        Some(_) => configured.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_for_legacy_versions() {
        for (v, j) in [
            ("1.14.4", 8),
            ("1.16.5", 8),
            ("1.17", 17),
            ("1.17.1", 17),
            ("1.18.2", 17),
            ("1.19.4", 17),
            ("1.20", 17),
            ("1.20.4", 17),
            ("1.20.5", 21),
            ("1.20.6", 21),
            ("1.21", 21),
            ("1.21.4", 21),
            ("1.22", 21),
        ] {
            assert_eq!(java_major_for(v), Some(j), "{v}");
        }
    }

    #[test]
    fn java_for_year_based_versions() {
        assert_eq!(java_major_for("26.1"), Some(25));
        assert_eq!(java_major_for("26.3"), Some(25));
        assert_eq!(java_major_for("26.3.1"), Some(25));
        assert_eq!(java_major_for("27.1"), Some(25));
    }

    #[test]
    fn unknown_formats_are_not_guessed() {
        for v in [
            "",
            "latest",
            "25w14a",
            "1.x",
            "1.21.4-pre1",
            "1.21.4.9",
            "26.3-rc1",
            "abc",
        ] {
            assert_eq!(java_major_for(v), None, "{v:?}");
        }
    }

    #[test]
    fn latest_tag_is_pinned_to_the_right_java() {
        let img = "docker.io/itzg/minecraft-server:latest";
        assert_eq!(
            resolve_image(img, "1.20.1"),
            "docker.io/itzg/minecraft-server:java17"
        );
        assert_eq!(
            resolve_image(img, "1.21.4"),
            "docker.io/itzg/minecraft-server:java21"
        );
        assert_eq!(
            resolve_image(img, "26.3"),
            "docker.io/itzg/minecraft-server:java25"
        );
        assert_eq!(
            resolve_image("docker.io/itzg/minecraft-server", "1.16.5"),
            "docker.io/itzg/minecraft-server:java8"
        );
    }

    #[test]
    fn pinned_images_and_unknown_versions_are_left_alone() {
        let pinned = "docker.io/itzg/minecraft-server:java21";
        assert_eq!(resolve_image(pinned, "26.3"), pinned);
        let digest = "docker.io/itzg/minecraft-server@sha256:abc";
        assert_eq!(resolve_image(digest, "1.20.1"), digest);
        let latest = "docker.io/itzg/minecraft-server:latest";
        assert_eq!(resolve_image(latest, "25w14a"), latest);
    }

    #[test]
    fn registry_port_is_not_mistaken_for_a_tag() {
        assert_eq!(
            resolve_image("registry.local:5000/itzg/minecraft-server", "1.20.1"),
            "registry.local:5000/itzg/minecraft-server:java17"
        );
        assert_eq!(
            resolve_image("registry.local:5000/itzg/minecraft-server:latest", "26.3"),
            "registry.local:5000/itzg/minecraft-server:java25"
        );
    }
}
