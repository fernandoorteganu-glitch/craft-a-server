//! Which `server.properties` settings an imported server may bring along.
//!
//! The file in an imported archive is untrusted. Only a short list of
//! gameplay settings is copied, each checked against its allowed values. Every
//! setting that the daemon manages (online mode, whitelist, RCON, ports, the
//! world name, the view distance) is never taken from the archive.

/// Settings copied from an imported file, with the check each value must pass.
const ALLOWED: &[(&str, Kind)] = &[
    (
        "difficulty",
        Kind::OneOf(&["peaceful", "easy", "normal", "hard"]),
    ),
    (
        "gamemode",
        Kind::OneOf(&["survival", "creative", "adventure", "spectator"]),
    ),
    ("force-gamemode", Kind::Bool),
    ("hardcore", Kind::Bool),
    ("pvp", Kind::Bool),
    ("allow-flight", Kind::Bool),
    ("allow-nether", Kind::Bool),
    ("spawn-monsters", Kind::Bool),
    ("spawn-animals", Kind::Bool),
    ("spawn-npcs", Kind::Bool),
    ("generate-structures", Kind::Bool),
    ("enable-command-block", Kind::Bool),
    ("max-players", Kind::Range(1, 500)),
    ("spawn-protection", Kind::Range(0, 1000)),
    ("simulation-distance", Kind::Range(3, 32)),
    ("player-idle-timeout", Kind::Range(0, 100_000)),
    ("max-world-size", Kind::Range(1, 29_999_984)),
    ("motd", Kind::Text(120)),
    ("level-seed", Kind::Seed),
];

#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Range(u64, u64),
    OneOf(&'static [&'static str]),
    Text(usize),
    Seed,
}

impl Kind {
    fn accepts(self, value: &str) -> bool {
        match self {
            Kind::Bool => matches!(value, "true" | "false"),
            Kind::Range(lo, hi) => value.parse::<u64>().is_ok_and(|n| (lo..=hi).contains(&n)),
            Kind::OneOf(options) => options.contains(&value),
            Kind::Text(max) => {
                value.chars().count() <= max && value.chars().all(|c| !c.is_control())
            }
            Kind::Seed => {
                value.len() <= 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b' '))
            }
        }
    }
}

/// `key=value` pairs of a properties file, comments and malformed lines dropped.
fn pairs(text: &str) -> Vec<(&str, &str)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            if line.trim_start().starts_with('#') {
                return None;
            }
            line.split_once('=')
        })
        .collect()
}

/// The live file with the allowed settings of `imported` applied.
///
/// Returns the new text and the names of the settings that were applied.
/// Unknown, managed and invalid settings are left out.
pub fn merge(live: &str, imported: &str) -> (String, Vec<String>) {
    let wanted: Vec<(&str, &str)> = pairs(imported)
        .into_iter()
        .filter(|(key, value)| {
            ALLOWED
                .iter()
                .any(|(name, kind)| name == key && kind.accepts(value))
        })
        .collect();

    let mut applied = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut out = String::with_capacity(live.len() + 128);
    for line in live.lines() {
        let line = line.trim_end_matches('\r');
        let replacement = line
            .split_once('=')
            .filter(|_| !line.trim_start().starts_with('#'))
            .and_then(|(key, _)| wanted.iter().rev().find(|(k, _)| *k == key));
        match replacement {
            Some((key, value)) => {
                out.push_str(key);
                out.push('=');
                out.push_str(value);
                if seen.insert(*key) {
                    applied.push((*key).to_owned());
                }
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    for (key, value) in &wanted {
        if seen.contains(key) {
            continue;
        }
        // Last value wins for a key that appears twice.
        let Some((_, last)) = wanted.iter().rev().find(|(k, _)| k == key) else {
            continue;
        };
        out.push_str(key);
        out.push('=');
        out.push_str(last);
        out.push('\n');
        seen.insert(*key);
        applied.push((*key).to_owned());
        let _ = value;
    }
    (out, applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_allowed_settings_and_nothing_else() {
        let live =
            "#Minecraft server properties\nonline-mode=true\ndifficulty=easy\nview-distance=10\n";
        let imported = "online-mode=false\nwhite-list=false\nenable-rcon=true\nrcon.password=x\n\
                        server-port=1\nlevel-name=../../etc\ndifficulty=hard\nview-distance=32\npvp=false\n";
        let (text, applied) = merge(live, imported);
        assert!(text.contains("online-mode=true"), "{text}");
        assert!(text.contains("view-distance=10"), "{text}");
        assert!(text.contains("difficulty=hard"), "{text}");
        assert!(text.contains("pvp=false"), "{text}");
        for banned in [
            "white-list=false",
            "enable-rcon",
            "rcon.password",
            "server-port",
            "level-name",
        ] {
            assert!(!text.contains(banned), "{banned} leaked: {text}");
        }
        assert_eq!(applied, vec!["difficulty".to_owned(), "pvp".to_owned()]);
    }

    #[test]
    fn invalid_values_are_ignored() {
        let live = "difficulty=easy\nmax-players=20\n";
        let imported =
            "difficulty=impossible\nmax-players=99999\nmotd=a\u{7}b\nlevel-seed=$(reboot)\n";
        let (text, applied) = merge(live, imported);
        assert_eq!(text, live);
        assert!(applied.is_empty());
    }

    #[test]
    fn last_duplicate_wins_and_new_keys_are_appended() {
        let (text, applied) = merge("a=b\n", "pvp=true\npvp=false\nmotd=Hello there\n");
        assert!(
            text.contains("pvp=false") && !text.contains("pvp=true"),
            "{text}"
        );
        assert!(text.contains("motd=Hello there"), "{text}");
        assert_eq!(applied.len(), 2);
    }

    #[test]
    fn comments_are_not_settings() {
        let (text, applied) = merge("pvp=true\n", "# pvp=false\n");
        assert_eq!(text, "pvp=true\n");
        assert!(applied.is_empty());
    }
}
