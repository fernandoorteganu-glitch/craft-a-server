use std::fs;
use std::io::Write;
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A minimal gzip header: all the import checks on a world's level.dat.
const LEVEL_DAT: &[u8] = &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1, 2, 3];

fn zip_bytes(files: &[(&str, &[u8])]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, data) in files {
        writer.start_file(*name, options)?;
        writer.write_all(data)?;
    }
    Ok(writer.finish()?.into_inner())
}

fn fabric_jar() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    zip_bytes(&[("fabric.mod.json", b"{}")])
}

fn icon(width: u32, height: u32) -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png
}

fn write_zip(
    dir: &Path,
    files: &[(&str, &[u8])],
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let path = dir.join("upload.zip");
    fs::write(&path, zip_bytes(files)?)?;
    Ok(path)
}

fn live_server(dir: &Path) -> TestResult {
    fs::create_dir_all(dir.join("world/region"))?;
    fs::write(dir.join("world/level.dat"), b"old world")?;
    fs::create_dir_all(dir.join("mods"))?;
    fs::write(dir.join("mods/old.jar"), b"old mod")?;
    fs::create_dir_all(dir.join("libraries/x"))?;
    fs::write(dir.join("libraries/x/lib.jar"), b"lib")?;
    fs::write(dir.join("fabric-server-launch.jar"), b"launcher")?;
    fs::write(dir.join("eula.txt"), b"eula=true\n")?;
    fs::write(dir.join("whitelist.json"), b"[{\"name\":\"old\"}]")?;
    fs::write(dir.join("ops.json"), b"[]")?;
    fs::create_dir_all(dir.join("logs"))?;
    fs::write(dir.join("logs/latest.log"), b"log")?;
    fs::write(
        dir.join("server.properties"),
        b"online-mode=true\nwhite-list=true\ndifficulty=easy\nview-distance=10\n",
    )?;
    Ok(())
}

#[test]
fn imports_a_whole_server_folder() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let jar = fabric_jar()?;
    let png = icon(64, 64);
    let zip = write_zip(
        tmp.path(),
        &[
            ("MyServer/world/level.dat", LEVEL_DAT),
            ("MyServer/world/region/r.0.0.mca", b"region"),
            ("MyServer/world/playerdata/a.dat", b"player"),
            ("MyServer/world/session.lock", b"lock"),
            ("MyServer/mods/Cool Mod (1.0).jar", &jar),
            ("MyServer/mods/readme.txt", b"hi"),
            ("MyServer/config/cool.toml", b"a=1"),
            ("MyServer/config/sub/deep.json", b"{}"),
            ("MyServer/whitelist.json", b"[{\"name\":\"friend\"}]"),
            ("MyServer/ops.json", b"{\"not\":\"a list\"}"),
            ("MyServer/server-icon.png", &png),
            (
                "MyServer/server.properties",
                b"online-mode=false\nwhite-list=false\ndifficulty=hard\nview-distance=32\nmotd=Hi\n",
            ),
            ("MyServer/libraries/evil.jar", b"x"),
            ("MyServer/server.jar", b"x"),
            ("MyServer/eula.txt", b"eula=false"),
            ("MyServer/logs/latest.log", b"x"),
        ],
    )?;
    let live = tmp.path().join("data");
    live_server(&live)?;

    let plan = inspect(&zip, &Limits::default())?;
    assert_eq!(plan.world_files, 3);
    assert_eq!(plan.mod_names(), vec!["Cool_Mod__1.0_.jar"]);
    assert!(plan.replaces_mods && plan.icon && plan.properties);
    assert_eq!(plan.config_files, 2);
    assert_eq!(plan.lists, vec!["whitelist.json", "ops.json"]);

    let staged = tmp.path().join("staged");
    let report = build_staged(&zip, &plan, &live, &staged, &Limits::default())?;

    // Imported.
    assert_eq!(fs::read(staged.join("world/level.dat"))?, LEVEL_DAT);
    assert_eq!(fs::read(staged.join("world/region/r.0.0.mca"))?, b"region");
    assert!(!staged.join("world/session.lock").exists());
    assert!(staged.join("mods/Cool_Mod__1.0_.jar").is_file());
    assert!(!staged.join("mods/old.jar").exists());
    assert_eq!(fs::read(staged.join("config/sub/deep.json"))?, b"{}");
    assert_eq!(
        fs::read(staged.join("whitelist.json"))?,
        b"[{\"name\":\"friend\"}]"
    );
    assert!(staged.join("server-icon.png").is_file());
    // The invalid list was refused and the live one is kept.
    assert_eq!(fs::read(staged.join("ops.json"))?, b"[]");
    assert!(report.notes.iter().any(|n| n.contains("ops.json")));
    // Settings: gameplay taken, managed ones not.
    let props = fs::read_to_string(staged.join("server.properties"))?;
    assert!(
        props.contains("difficulty=hard") && props.contains("motd=Hi"),
        "{props}"
    );
    assert!(
        props.contains("online-mode=true") && props.contains("white-list=true"),
        "{props}"
    );
    assert!(props.contains("view-distance=10"), "{props}");
    // Server runtime files come from the live server, never from the zip.
    assert_eq!(fs::read(staged.join("libraries/x/lib.jar"))?, b"lib");
    assert!(!staged.join("libraries/evil.jar").exists());
    assert_eq!(
        fs::read(staged.join("fabric-server-launch.jar"))?,
        b"launcher"
    );
    assert_eq!(fs::read(staged.join("eula.txt"))?, b"eula=true\n");
    assert!(!staged.join("logs").exists());
    assert!(!staged.join("server.jar").exists());

    // The live directory was not touched.
    assert_eq!(fs::read(live.join("world/level.dat"))?, b"old world");
    assert_eq!(fs::read(live.join("mods/old.jar"))?, b"old mod");
    assert!(fs::read_to_string(live.join("server.properties"))?.contains("difficulty=easy"));
    Ok(())
}

#[test]
fn a_world_only_zip_keeps_the_servers_mods_and_lists() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(
        tmp.path(),
        &[("level.dat", LEVEL_DAT), ("region/r.0.0.mca", b"region")],
    )?;
    let live = tmp.path().join("data");
    live_server(&live)?;
    let plan = inspect(&zip, &Limits::default())?;
    assert!(!plan.replaces_mods);
    let staged = tmp.path().join("staged");
    build_staged(&zip, &plan, &live, &staged, &Limits::default())?;
    assert_eq!(fs::read(staged.join("world/region/r.0.0.mca"))?, b"region");
    assert_eq!(fs::read(staged.join("mods/old.jar"))?, b"old mod");
    assert_eq!(
        fs::read(staged.join("whitelist.json"))?,
        b"[{\"name\":\"old\"}]"
    );
    Ok(())
}

#[test]
fn a_zip_with_just_the_world_folder_works() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(
        tmp.path(),
        &[
            ("My World/level.dat", LEVEL_DAT),
            ("My World/region/r.1.1.mca", b"r"),
        ],
    )?;
    let plan = inspect(&zip, &Limits::default())?;
    assert_eq!(plan.world_files, 2);
    let live = tmp.path().join("data");
    live_server(&live)?;
    let staged = tmp.path().join("staged");
    build_staged(&zip, &plan, &live, &staged, &Limits::default())?;
    assert_eq!(fs::read(staged.join("world/region/r.1.1.mca"))?, b"r");
    Ok(())
}

#[test]
fn backslash_paths_from_windows_tools_are_understood() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(
        tmp.path(),
        &[
            ("srv\\world\\level.dat", LEVEL_DAT),
            ("srv\\world\\region\\r.0.0.mca", b"r"),
        ],
    )?;
    let plan = inspect(&zip, &Limits::default())?;
    assert_eq!(plan.world_files, 2);
    Ok(())
}

#[test]
fn unsafe_paths_are_refused() -> TestResult {
    let tmp = tempfile::tempdir()?;
    for bad in [
        "../evil.txt",
        "world/../../evil",
        "/etc/passwd",
        "world/a:b",
        "a//b",
    ] {
        let zip = write_zip(tmp.path(), &[("world/level.dat", LEVEL_DAT), (bad, b"x")])?;
        let result = inspect(&zip, &Limits::default());
        assert!(
            matches!(result, Err(WorldError::Invalid(_))),
            "{bad} was accepted: {result:?}"
        );
    }
    Ok(())
}

#[test]
fn nothing_is_written_outside_the_staged_directory() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(
        tmp.path(),
        &[("world/level.dat", LEVEL_DAT), ("../escape", b"x")],
    )?;
    assert!(inspect(&zip, &Limits::default()).is_err());
    assert!(!tmp.path().join("escape").exists());
    Ok(())
}

#[test]
fn a_zip_without_a_world_is_refused() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(tmp.path(), &[("mods/a.jar", b"x"), ("readme.txt", b"x")])?;
    let result = inspect(&zip, &Limits::default());
    assert!(matches!(result, Err(WorldError::Invalid(m)) if m.contains("level.dat")));
    Ok(())
}

#[test]
fn garbage_and_missing_files_are_refused() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("upload.zip");
    fs::write(&path, b"this is not a zip")?;
    assert!(matches!(
        inspect(&path, &Limits::default()),
        Err(WorldError::Invalid(_))
    ));
    assert!(inspect(&tmp.path().join("missing.zip"), &Limits::default()).is_err());
    Ok(())
}

#[test]
fn a_level_dat_that_is_not_gzip_fails_and_leaves_nothing() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(tmp.path(), &[("world/level.dat", b"plain text")])?;
    let plan = inspect(&zip, &Limits::default())?;
    let live = tmp.path().join("data");
    live_server(&live)?;
    let staged = tmp.path().join("staged");
    let result = build_staged(&zip, &plan, &live, &staged, &Limits::default());
    assert!(matches!(result, Err(WorldError::Invalid(_))));
    assert!(!staged.exists());
    assert_eq!(fs::read(live.join("world/level.dat"))?, b"old world");
    Ok(())
}

#[test]
fn mods_that_are_not_fabric_mods_are_skipped_with_a_note() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let forge = zip_bytes(&[("META-INF/mods.toml", b"x")])?;
    let good = fabric_jar()?;
    let zip = write_zip(
        tmp.path(),
        &[
            ("world/level.dat", LEVEL_DAT),
            ("mods/forge-only.jar", &forge),
            ("mods/fabric-ok.jar", &good),
            ("mods/not-a-zip.jar", b"nope"),
        ],
    )?;
    let plan = inspect(&zip, &Limits::default())?;
    let live = tmp.path().join("data");
    live_server(&live)?;
    let staged = tmp.path().join("staged");
    let report = build_staged(&zip, &plan, &live, &staged, &Limits::default())?;
    assert_eq!(report.mods_imported, vec!["fabric-ok.jar".to_owned()]);
    assert_eq!(report.notes.len(), 2);
    assert!(staged.join("mods/fabric-ok.jar").is_file());
    assert!(!staged.join("mods/forge-only.jar").exists());
    Ok(())
}

#[test]
fn the_icon_must_be_a_64_pixel_png() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let wrong = icon(128, 128);
    let zip = write_zip(
        tmp.path(),
        &[("world/level.dat", LEVEL_DAT), ("server-icon.png", &wrong)],
    )?;
    let plan = inspect(&zip, &Limits::default())?;
    let live = tmp.path().join("data");
    live_server(&live)?;
    let staged = tmp.path().join("staged");
    let report = build_staged(&zip, &plan, &live, &staged, &Limits::default())?;
    assert!(!report.icon && !staged.join("server-icon.png").exists());
    assert!(report.notes.iter().any(|n| n.contains("64x64")));
    Ok(())
}

#[test]
fn size_limits_are_enforced() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(
        tmp.path(),
        &[
            ("world/level.dat", LEVEL_DAT),
            ("world/region/big.mca", &[0_u8; 4096]),
        ],
    )?;
    let small = Limits {
        max_total_bytes: 1024,
        ..Limits::default()
    };
    assert!(matches!(inspect(&zip, &small), Err(WorldError::Invalid(_))));
    let tiny_zip = Limits {
        max_zip_bytes: 10,
        ..Limits::default()
    };
    assert!(matches!(
        inspect(&zip, &tiny_zip),
        Err(WorldError::Invalid(_))
    ));
    Ok(())
}

#[test]
fn more_than_one_world_is_ambiguous_unless_one_is_called_world() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(
        tmp.path(),
        &[("a/level.dat", LEVEL_DAT), ("b/level.dat", LEVEL_DAT)],
    )?;
    assert!(inspect(&zip, &Limits::default()).is_err());
    let zip = write_zip(
        tmp.path(),
        &[
            ("srv/world/level.dat", LEVEL_DAT),
            ("srv/old/level.dat", LEVEL_DAT),
        ],
    )?;
    let plan = inspect(&zip, &Limits::default())?;
    assert_eq!(
        (plan.base.as_str(), plan.world_dir.as_str()),
        ("srv", "world")
    );
    Ok(())
}

#[test]
fn a_nested_backup_world_does_not_win_over_the_real_one() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let zip = write_zip(
        tmp.path(),
        &[
            ("world/level.dat", LEVEL_DAT),
            ("backups/world/level.dat", LEVEL_DAT),
            ("backups/world/region/r.0.0.mca", b"old"),
        ],
    )?;
    let plan = inspect(&zip, &Limits::default())?;
    assert_eq!(plan.world_files, 1);
    Ok(())
}

#[test]
fn jar_names_are_made_safe_and_unique() {
    let mut taken = HashSet::new();
    assert_eq!(
        safe_jar_name("Sodium (1).jar", &mut taken),
        "Sodium__1_.jar"
    );
    assert_eq!(
        safe_jar_name("sodium__1_.JAR", &mut taken),
        "sodium__1_-2.jar"
    );
    assert_eq!(safe_jar_name(".hidden.jar", &mut taken), "_.hidden.jar");
    assert_eq!(safe_jar_name("a b/c.jar", &mut taken), "a_b_c.jar");
    assert!(safe_jar_name(&format!("{}.jar", "x".repeat(400)), &mut taken).len() <= 120);
}

#[test]
fn classification() {
    assert_eq!(
        classify("world/region/r.mca", "world"),
        Class::World("region/r.mca".into())
    );
    assert_eq!(classify("world/session.lock", "world"), Class::Ignore);
    assert_eq!(classify("mods/a.JAR", "world"), Class::Mod);
    assert_eq!(classify("mods/sub/a.jar", "world"), Class::Ignore);
    assert_eq!(
        classify("config/a/b.toml", "world"),
        Class::Config("a/b.toml".into())
    );
    assert_eq!(
        classify("whitelist.json", "world"),
        Class::List("whitelist.json")
    );
    assert_eq!(classify("plugins/x.jar", "world"), Class::Ignore);
    assert_eq!(
        classify("anything/at/all", ""),
        Class::World("anything/at/all".into())
    );
}
