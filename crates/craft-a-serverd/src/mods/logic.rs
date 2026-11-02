//! Small pure helpers for mod changes.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::resolver::Conflict;

/// Text from the network or the filesystem made safe to show: control
/// characters and bidi overrides dropped, length capped (in characters).
pub fn clean_text(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .filter(|c| !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(max_chars)
        .collect()
}

/// A file name for `wanted` that does not clash with `taken` (lowercase names
/// already in the mods directory). `None` after too many collisions.
pub fn unique_file_name(wanted: &str, project_id: &str, taken: &HashSet<String>) -> Option<String> {
    let is_taken = |n: &str| taken.contains(&n.to_ascii_lowercase());
    if !is_taken(wanted) {
        return Some(wanted.to_owned());
    }
    let stem = wanted
        .get(..wanted.len().saturating_sub(".jar".len()))
        .unwrap_or(wanted);
    let first = format!("{stem}-{project_id}.jar");
    if !is_taken(&first) {
        return Some(first);
    }
    (2..100)
        .map(|n| format!("{stem}-{project_id}-{n}.jar"))
        .find(|n| !is_taken(n))
}

/// Dependencies that become useless once `targets` are removed: installed as a
/// dependency, required by something being removed, and needed by nothing that
/// stays. Follows chains (a dependency of a dependency).
///
/// `requires` maps a project to the projects it requires; `as_dependency`
/// says which installed projects were pulled in automatically.
pub fn orphans_after_removal(
    targets: &[String],
    as_dependency: &HashMap<String, bool>,
    requires: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (project, needs) in requires {
        for need in needs {
            dependents
                .entry(need.as_str())
                .or_default()
                .push(project.as_str());
        }
    }
    let mut gone: BTreeSet<String> = targets.iter().cloned().collect();
    loop {
        let mut added = Vec::new();
        for removed in &gone {
            for dep in requires.get(removed).into_iter().flatten() {
                let automatic = as_dependency.get(dep).copied().unwrap_or(false);
                let needed_elsewhere = dependents
                    .get(dep.as_str())
                    .into_iter()
                    .flatten()
                    .any(|d| !gone.contains(*d));
                if automatic && !gone.contains(dep) && !needed_elsewhere {
                    added.push(dep.clone());
                }
            }
        }
        if added.is_empty() {
            break;
        }
        gone.extend(added);
    }
    gone.into_iter().filter(|p| !targets.contains(p)).collect()
}

/// Installed projects (outside `removed`) that require something in `removed`.
pub fn dependents_outside(
    removed: &[String],
    requires: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for (project, needs) in requires {
        if !removed.contains(project) && needs.iter().any(|n| removed.contains(n)) {
            out.insert(project.clone());
        }
    }
    out.into_iter().collect()
}

/// A sentence for a conflict; `title` turns a project id into a display name.
pub fn describe_conflict(conflict: &Conflict, title: &dyn Fn(&str) -> String) -> String {
    match conflict {
        Conflict::Incompatible { project, with } => format!(
            "{} says it does not work together with {}",
            title(project),
            title(with)
        ),
        Conflict::VersionPinned {
            project,
            wanted_version,
            have_version,
        } => format!(
            "something needs build {wanted_version} of {} but build {have_version} is the one in use",
            title(project)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_ascii_lowercase()).collect()
    }

    #[test]
    fn display_text_loses_controls_and_is_capped() {
        assert_eq!(clean_text("a\u{0}b\nc\u{202e}d", 10), "abcd");
        assert_eq!(clean_text(&"x".repeat(50), 5), "xxxxx");
        // HTML stays as text; the templates escape it.
        assert_eq!(clean_text("<script>", 20), "<script>");
    }

    #[test]
    fn names_stay_unique() {
        assert_eq!(
            unique_file_name("a.jar", "P1", &set(&[])).as_deref(),
            Some("a.jar")
        );
        assert_eq!(
            unique_file_name("a.jar", "P1", &set(&["A.JAR"])).as_deref(),
            Some("a-P1.jar")
        );
        assert_eq!(
            unique_file_name("a.jar", "P1", &set(&["a.jar", "a-P1.jar"])).as_deref(),
            Some("a-P1-2.jar")
        );
        let many: Vec<String> = std::iter::once("a.jar".to_owned())
            .chain(std::iter::once("a-p1.jar".to_owned()))
            .chain((2..100).map(|n| format!("a-p1-{n}.jar")))
            .collect();
        let taken: HashSet<String> = many.into_iter().collect();
        assert_eq!(unique_file_name("a.jar", "P1", &taken), None);
    }

    fn graph(
        edges: &[(&str, &[&str])],
        automatic: &[&str],
    ) -> (HashMap<String, bool>, HashMap<String, Vec<String>>) {
        let mut as_dep = HashMap::new();
        let mut requires = HashMap::new();
        for (p, needs) in edges {
            as_dep.insert((*p).to_owned(), automatic.contains(p));
            requires.insert(
                (*p).to_owned(),
                needs.iter().map(|s| (*s).to_owned()).collect(),
            );
        }
        (as_dep, requires)
    }

    #[test]
    fn orphans_follow_chains_but_keep_shared_and_explicit_mods() {
        // root -> lib -> core ; other -> core ; root -> api (explicitly installed too)
        let (as_dep, requires) = graph(
            &[
                ("root", &["lib", "api"]),
                ("lib", &["core"]),
                ("core", &[]),
                ("other", &["core"]),
                ("api", &[]),
            ],
            &["lib", "core"],
        );
        // core is still needed by "other"; api was installed by the user.
        assert_eq!(
            orphans_after_removal(&["root".to_owned()], &as_dep, &requires),
            ["lib"]
        );
        // Without "other", core follows lib.
        let (as_dep, requires) = graph(
            &[("root", &["lib"]), ("lib", &["core"]), ("core", &[])],
            &["lib", "core"],
        );
        assert_eq!(
            orphans_after_removal(&["root".to_owned()], &as_dep, &requires),
            ["core", "lib"]
        );
        assert!(orphans_after_removal(&["core".to_owned()], &as_dep, &requires).is_empty());
    }

    #[test]
    fn cycles_terminate() {
        let (as_dep, requires) = graph(
            &[("root", &["a"]), ("a", &["b"]), ("b", &["a"])],
            &["a", "b"],
        );
        // a and b keep each other "needed" only inside the cycle; both go with root.
        let got = orphans_after_removal(&["root".to_owned()], &as_dep, &requires);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn dependents_are_found_outside_the_removed_set() {
        let (_, requires) = graph(
            &[("root", &["lib"]), ("lib", &[]), ("other", &["lib"])],
            &[],
        );
        assert_eq!(
            dependents_outside(&["lib".to_owned()], &requires),
            ["other", "root"]
        );
        assert_eq!(
            dependents_outside(&["lib".to_owned(), "root".to_owned()], &requires),
            ["other"]
        );
    }

    #[test]
    fn conflicts_read_well() {
        let title = |id: &str| format!("<{id}>");
        let c = Conflict::Incompatible {
            project: "a".into(),
            with: "b".into(),
        };
        assert_eq!(
            describe_conflict(&c, &title),
            "<a> says it does not work together with <b>"
        );
    }
}
