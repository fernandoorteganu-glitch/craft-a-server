// Test code: a wrong shape should fail loudly.
#![allow(clippy::indexing_slicing, clippy::panic)]

//! Resolver tests: recorded Modrinth fixtures plus synthetic graphs.

use super::*;
use crate::mods::testutil::{MapSource, Synth, versions_fixture};

const FABRIC_API: &str = "P7dR8mSH";
const OPTS: Options<'static> = Options {
    mc_version: "26.3",
    allow_prerelease: false,
};

fn fixtures_source() -> MapSource {
    let mut s = MapSource::default();
    for name in [
        "fabric-api",
        "veinminer",
        "jade",
        "lambdynamiclights",
        "iris",
        "sodium",
        "modernfix",
    ] {
        s.add(versions_fixture(&format!(
            "versions_{name}_fabric_26.3.json"
        )));
    }
    // fabric-language-kotlin has no fixture; a synthetic build stands in.
    s.add(vec![Synth::new("Ha28R6CL", "FLKver01").version()]);
    s
}

fn names(plan: &Plan) -> Vec<&str> {
    plan.to_install
        .iter()
        .map(|m| m.project_id.as_str())
        .collect()
}

fn installed(project: &str, version: &str) -> InstalledRef {
    InstalledRef {
        project_id: project.into(),
        version_id: version.into(),
        file_name: format!("{project}.jar"),
    }
}

#[test]
fn required_dependencies_come_first_and_optional_ones_are_only_suggested() -> Result<(), ModError> {
    let plan = resolve(&fixtures_source(), OPTS, &[], "OhduvhIc")?; // veinminer
    assert_eq!(names(&plan), [FABRIC_API, "Ha28R6CL", "OhduvhIc"]);
    let root = plan.to_install.last().ok_or(ModError::NotConfigured)?;
    assert!(!root.as_dependency);
    assert_eq!(root.version_id, "QMYktnGo");
    assert_eq!(root.requires, ["Ha28R6CL", FABRIC_API]);
    assert!(plan.to_install.iter().take(2).all(|m| m.as_dependency));
    assert_eq!(plan.optional, ["dxa0Bm8m"]);
    assert!(plan.conflicts.is_empty());
    Ok(())
}

#[test]
fn the_newest_release_wins_and_sources_jars_are_never_chosen() -> Result<(), ModError> {
    let plan = resolve(&fixtures_source(), OPTS, &[], "nvQzSEkH")?; // jade: 2 files per version
    let jade = plan.to_install.last().ok_or(ModError::NotConfigured)?;
    assert_eq!(jade.version_id, "71CTWqdE");
    assert_eq!(jade.file.file_name, "Jade-mc26.3-Fabric-26.3.5.jar");
    assert_eq!(plan.optional, ["u6dRKJwZ"]); // jei
    assert!(names(&plan).contains(&FABRIC_API));
    Ok(())
}

#[test]
fn an_installed_dependency_is_satisfied_not_planned_again() -> Result<(), ModError> {
    let have = [installed(FABRIC_API, "LEklt8GT")]; // an older fabric-api
    let plan = resolve(&fixtures_source(), OPTS, &have, "nvQzSEkH")?;
    assert_eq!(names(&plan), ["nvQzSEkH"]);
    assert_eq!(plan.satisfied, [FABRIC_API]);
    Ok(())
}

#[test]
fn optional_dependencies_that_are_installed_are_not_suggested() -> Result<(), ModError> {
    let have = [installed("u6dRKJwZ", "x"), installed(FABRIC_API, "y")];
    let plan = resolve(&fixtures_source(), OPTS, &have, "nvQzSEkH")?;
    assert!(plan.optional.is_empty());
    Ok(())
}

#[test]
fn incompatible_blocks_only_when_the_other_mod_is_present() -> Result<(), ModError> {
    let src = fixtures_source();
    let clean = resolve(&src, OPTS, &[], "yBW8D80W")?; // lambdynamiclights
    assert!(clean.conflicts.is_empty());
    let have = [installed("PxQSWIcD", "v")];
    let plan = resolve(&src, OPTS, &have, "yBW8D80W")?;
    assert_eq!(
        plan.conflicts,
        [Conflict::Incompatible {
            project: "yBW8D80W".into(),
            with: "PxQSWIcD".into()
        }]
    );
    Ok(())
}

#[test]
fn a_pinned_version_id_is_used_even_when_it_is_a_beta() -> Result<(), ModError> {
    let plan = resolve(&fixtures_source(), OPTS, &[], "YL57xq9U")?; // iris -> sodium bAZQdGpg
    let sodium = plan
        .to_install
        .iter()
        .find(|m| m.project_id == "AANobbMI")
        .ok_or(ModError::NotConfigured)?;
    assert_eq!(sodium.version_id, "bAZQdGpg");
    assert!(sodium.as_dependency);
    Ok(())
}

#[test]
fn a_mod_without_a_fabric_build_for_this_version_says_so() {
    let err = resolve(&fixtures_source(), OPTS, &[], "modernfix_project").err();
    // Unknown project: empty list.
    assert!(matches!(
        err,
        Some(ModError::NoCompatibleVersion { hint: "", .. })
    ));
    let mf = versions_fixture("versions_modernfix_fabric_26.3.json");
    assert!(mf.is_empty());
}

#[test]
fn beta_only_projects_need_explicit_consent() -> Result<(), ModError> {
    let src = fixtures_source();
    // Sodium has a newer beta, but a release exists: the release wins.
    let plan = resolve(&src, OPTS, &[], "AANobbMI")?;
    assert_eq!(
        plan.to_install
            .last()
            .ok_or(ModError::NotConfigured)?
            .version_id,
        "bAZQdGpg"
    );
    let mut s = MapSource::default();
    s.add(vec![Synth::new("AAAAAAAA", "a1").kind("beta").version()]);
    match resolve(&s, OPTS, &[], "AAAAAAAA").err() {
        Some(ModError::NoCompatibleVersion { hint, .. }) => assert!(hint.contains("beta")),
        other => panic!("{other:?}"),
    }
    let plan = resolve(
        &s,
        Options {
            allow_prerelease: true,
            ..OPTS
        },
        &[],
        "AAAAAAAA",
    )?;
    assert_eq!(plan.to_install.len(), 1);
    Ok(())
}

#[test]
fn dependency_cycles_terminate_and_install_each_mod_once() -> Result<(), ModError> {
    let mut s = MapSource::default();
    s.add(vec![
        Synth::new("AAAAAAAA", "a1")
            .dep("BBBBBBBB", "required")
            .version(),
    ]);
    s.add(vec![
        Synth::new("BBBBBBBB", "b1")
            .dep("CCCCCCCC", "required")
            .version(),
    ]);
    s.add(vec![
        Synth::new("CCCCCCCC", "c1")
            .dep("AAAAAAAA", "required")
            .version(),
    ]);
    let plan = resolve(&s, OPTS, &[], "AAAAAAAA")?;
    assert_eq!(names(&plan), ["CCCCCCCC", "BBBBBBBB", "AAAAAAAA"]);
    // A self-dependency too.
    let mut s = MapSource::default();
    s.add(vec![
        Synth::new("AAAAAAAA", "a1")
            .dep("AAAAAAAA", "required")
            .version(),
    ]);
    assert_eq!(names(&resolve(&s, OPTS, &[], "AAAAAAAA")?), ["AAAAAAAA"]);
    Ok(())
}

#[test]
fn diamonds_install_the_shared_dependency_once() -> Result<(), ModError> {
    let mut s = MapSource::default();
    s.add(vec![
        Synth::new("RRRRRRRR", "r1")
            .dep("LLLLLLLL", "required")
            .dep("MMMMMMMM", "required")
            .version(),
    ]);
    s.add(vec![
        Synth::new("LLLLLLLL", "l1")
            .dep("DDDDDDDD", "required")
            .version(),
    ]);
    s.add(vec![
        Synth::new("MMMMMMMM", "m1")
            .dep("DDDDDDDD", "required")
            .version(),
    ]);
    s.add(vec![Synth::new("DDDDDDDD", "d1").version()]);
    let plan = resolve(&s, OPTS, &[], "RRRRRRRR")?;
    assert_eq!(
        plan.to_install
            .iter()
            .filter(|m| m.project_id == "DDDDDDDD")
            .count(),
        1
    );
    assert_eq!(plan.to_install.len(), 4);
    assert!(plan.conflicts.is_empty());
    Ok(())
}

#[test]
fn two_different_pinned_builds_of_one_dependency_conflict() -> Result<(), ModError> {
    let mut s = MapSource::default();
    let mut root = Synth::new("RRRRRRRR", "r1");
    root.deps.push(("LLLLLLLL".into(), None, "required"));
    root.deps.push(("MMMMMMMM".into(), None, "required"));
    s.add(vec![root.version()]);
    let mut l = Synth::new("LLLLLLLL", "l1");
    l.deps
        .push(("DDDDDDDD".into(), Some("d1".into()), "required"));
    let mut m = Synth::new("MMMMMMMM", "m1");
    m.deps
        .push(("DDDDDDDD".into(), Some("d2".into()), "required"));
    s.add(vec![l.version(), m.version()]);
    s.add(vec![
        Synth::new("DDDDDDDD", "d1").version(),
        Synth::new("DDDDDDDD", "d2").version(),
    ]);
    let plan = resolve(&s, OPTS, &[], "RRRRRRRR")?;
    assert!(matches!(
        plan.conflicts.as_slice(),
        [Conflict::VersionPinned { project, .. }] if project == "DDDDDDDD"
    ));
    Ok(())
}

#[test]
fn embedded_and_unknown_dependency_types_are_ignored() -> Result<(), ModError> {
    let mut s = MapSource::default();
    s.add(vec![
        Synth::new("RRRRRRRR", "r1")
            .dep("EEEEEEEE", "embedded")
            .dep("UUUUUUUU", "something_new")
            .version(),
    ]);
    let plan = resolve(&s, OPTS, &[], "RRRRRRRR")?;
    assert_eq!(names(&plan), ["RRRRRRRR"]);
    assert!(s.calls.lock().map(|c| c.len()).unwrap_or(0) == 1);
    Ok(())
}

#[test]
fn a_dependency_with_no_ids_is_counted_not_followed() -> Result<(), ModError> {
    let mut s = MapSource::default();
    let mut v = Synth::new("RRRRRRRR", "r1").version();
    v.dependencies.push(Dependency {
        version_id: None,
        project_id: None,
        dependency_type: DependencyType::Required,
    });
    s.add(vec![v]);
    let plan = resolve(&s, OPTS, &[], "RRRRRRRR")?;
    assert_eq!(plan.unresolved, 1);
    Ok(())
}

#[test]
fn reinstalling_the_same_version_is_refused_and_a_newer_one_replaces() -> Result<(), ModError> {
    let mut s = MapSource::default();
    s.add(vec![
        Synth::new("RRRRRRRR", "r2").number("2.0").version(),
        {
            let mut old = Synth::new("RRRRRRRR", "r1");
            old.date = "2026-09-01T00:00:00Z".into();
            old.version()
        },
    ]);
    let same = [installed("RRRRRRRR", "r2")];
    assert!(matches!(
        resolve(&s, OPTS, &same, "RRRRRRRR").err(),
        Some(ModError::AlreadyInstalled(_))
    ));
    let older = [installed("RRRRRRRR", "r1")];
    let plan = resolve(&s, OPTS, &older, "RRRRRRRR")?;
    let root = plan.to_install.last().ok_or(ModError::NotConfigured)?;
    assert_eq!(root.version_id, "r2");
    assert_eq!(root.replaces.as_deref(), Some("RRRRRRRR.jar"));
    Ok(())
}

#[test]
fn too_many_projects_are_refused() {
    let mut s = MapSource::default();
    for i in 0..=MAX_PROJECTS {
        let id = format!("P{i:07}");
        let mut v = Synth::new(&id, &format!("V{i:07}"));
        if i < MAX_PROJECTS {
            v = v.dep(&format!("P{:07}", i + 1), "required");
        }
        s.add(vec![v.version()]);
    }
    assert!(matches!(
        resolve(&s, OPTS, &[], "P0000000").err(),
        Some(ModError::TooMany(MAX_PROJECTS))
    ));
}

// ------------------------------------------------ hostile responses

fn one(mut v: Version) -> MapSource {
    let mut s = MapSource::default();
    v.project_id = "RRRRRRRR".into();
    s.add(vec![v]);
    s
}

#[test]
fn a_download_url_off_the_cdn_is_refused_at_plan_time() {
    for bad in [
        "https://evil.example/x.jar",
        "http://cdn.modrinth.com/x.jar",
        "https://cdn.modrinth.com.evil.example/x.jar",
    ] {
        let mut v = Synth::new("RRRRRRRR", "r1").version();
        v.files[0].url = bad.into();
        let err = resolve(&one(v), OPTS, &[], "RRRRRRRR").err();
        assert!(
            matches!(err, Some(ModError::ForbiddenHost(_))),
            "{bad}: {err:?}"
        );
    }
}

#[test]
fn missing_or_malformed_checksums_and_odd_sizes_are_refused() {
    let mut v = Synth::new("RRRRRRRR", "r1").version();
    v.files[0].hashes.sha512 = None;
    assert!(matches!(
        resolve(&one(v), OPTS, &[], "RRRRRRRR").err(),
        Some(ModError::BadResponse(_))
    ));
    let mut v = Synth::new("RRRRRRRR", "r1").version();
    v.files[0].hashes.sha512 = Some("zz".into());
    assert!(matches!(
        resolve(&one(v), OPTS, &[], "RRRRRRRR").err(),
        Some(ModError::BadResponse(_))
    ));
    let mut v = Synth::new("RRRRRRRR", "r1").version();
    v.files[0].size = MAX_MOD_BYTES + 1;
    assert!(matches!(
        resolve(&one(v), OPTS, &[], "RRRRRRRR").err(),
        Some(ModError::TooLarge { .. })
    ));
    let mut v = Synth::new("RRRRRRRR", "r1").version();
    v.files[0].size = 0;
    assert!(resolve(&one(v), OPTS, &[], "RRRRRRRR").is_err());
}

#[test]
fn path_traversal_file_names_are_replaced_by_an_id_based_name() -> Result<(), ModError> {
    for bad in ["../x.jar", "..\\..\\evil.jar", "/etc/passwd.jar", "a/b.jar"] {
        let mut v = Synth::new("RRRRRRRR", "r1").version();
        v.files[0].filename = bad.into();
        let plan = resolve(&one(v), OPTS, &[], "RRRRRRRR")?;
        let root = plan.to_install.last().ok_or(ModError::NotConfigured)?;
        assert_eq!(root.file.file_name, "RRRRRRRR-r1.jar", "{bad}");
    }
    Ok(())
}

#[test]
fn hostile_ids_in_dependencies_never_reach_the_source() {
    for bad in ["../../etc", "a/b", "x y", ""] {
        let mut s = MapSource::default();
        let mut v = Synth::new("RRRRRRRR", "r1").version();
        v.dependencies.push(Dependency {
            version_id: None,
            project_id: Some(bad.into()),
            dependency_type: DependencyType::Required,
        });
        s.add(vec![v]);
        assert!(resolve(&s, OPTS, &[], "RRRRRRRR").is_err(), "{bad:?}");
        assert_eq!(s.calls.lock().map(|c| c.len()).unwrap_or(9), 1, "{bad:?}");
    }
    let mut v = Synth::new("RRRRRRRR", "r1").version();
    v.id = "../x".into();
    let s = one(v);
    assert!(resolve(&s, OPTS, &[], "RRRRRRRR").is_err());
}

#[test]
fn versions_for_other_loaders_games_or_unlisted_are_ignored() {
    let mut other_game = Synth::new("RRRRRRRR", "r1");
    other_game.mc = "1.20.1".into();
    let mut draft = Synth::new("RRRRRRRR", "r2").version();
    draft.status = "draft".into();
    let mut quilt = Synth::new("RRRRRRRR", "r3").version();
    quilt.loaders = vec!["quilt".into()];
    let mut s = MapSource::default();
    s.add(vec![other_game.version(), draft, quilt]);
    assert!(matches!(
        resolve(&s, OPTS, &[], "RRRRRRRR").err(),
        Some(ModError::NoCompatibleVersion { .. })
    ));
}
