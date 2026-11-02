//! Flow tests with fakes: no network, no processes. Signatures in the fixtures
//! come from the real `minisign` tool (see tests/fixtures/update/regen.sh).

use std::cell::RefCell;
use std::collections::HashMap;

use super::*;

const SUMS: &str = include_str!("../../tests/fixtures/update/SHA256SUMS");
const SIG: &str = include_str!("../../tests/fixtures/update/SHA256SUMS.minisig");
const SIG_B: &str = include_str!("../../tests/fixtures/update/SHA256SUMS.keyb.minisig");
const KEY_A: &str = include_str!("../../tests/fixtures/update/key_a.pub");
const ARCHIVE: &[u8] = include_bytes!(
    "../../tests/fixtures/update/craft-a-server-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
);
const ARCHIVE_NAME: &str = "craft-a-server-1.2.3-x86_64-unknown-linux-gnu.tar.gz";
const BASE: &str = "https://updates.example.test/craft";

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// URL -> bytes; records every request.
#[derive(Default)]
struct FakeFetch {
    routes: HashMap<String, Vec<u8>>,
    seen: RefCell<Vec<String>>,
}

impl FakeFetch {
    fn release() -> FakeFetch {
        let mut f = FakeFetch::default();
        f.put("SHA256SUMS", SUMS.as_bytes());
        f.put("SHA256SUMS.minisig", SIG.as_bytes());
        f.put(ARCHIVE_NAME, ARCHIVE);
        f
    }

    fn put(&mut self, name: &str, bytes: &[u8]) {
        self.routes.insert(format!("{BASE}/{name}"), bytes.to_vec());
    }

    fn fetched_archive(&self) -> bool {
        self.seen.borrow().iter().any(|u| u.ends_with(".tar.gz"))
    }
}

impl Fetch for FakeFetch {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        self.seen.borrow_mut().push(url.to_owned());
        let body = self.routes.get(url).ok_or(FetchError::Status(404))?;
        if body.len() as u64 > max_bytes {
            return Err(FetchError::TooLarge(max_bytes));
        }
        Ok(body.clone())
    }
}

/// One installer call: program, arguments, staged file names at that moment.
type Call = (PathBuf, Vec<OsString>, Vec<String>);

/// Records the arguments and what the staging directory held at that moment.
#[derive(Default)]
struct FakeRunner {
    calls: RefCell<Vec<Call>>,
    fail_with: Option<String>,
}

impl Runner for FakeRunner {
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<Option<String>> {
        let dir = args
            .get(2)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("no staging argument"))?;
        let mut names: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        self.calls
            .borrow_mut()
            .push((program.to_owned(), args.to_vec(), names));
        Ok(self.fail_with.clone())
    }
}

fn keys() -> Result<Vec<PublicKey>, KeyError> {
    keys::parse_keys(KEY_A)
}

fn settings(current: &str) -> Result<Settings, UpdateError> {
    Settings::new(BASE, keys(), "x86_64", current)
}

struct Env {
    _dir: tempfile::TempDir,
    paths: Paths,
}

fn env() -> Result<Env, io::Error> {
    let dir = tempfile::tempdir()?;
    let installer = dir.path().join("install.sh");
    std::fs::write(&installer, "#!/bin/sh\n")?;
    let paths = Paths {
        staging: dir.path().join("staging"),
        installer,
    };
    Ok(Env { _dir: dir, paths })
}

fn yes(_: &Checked) -> bool {
    true
}

#[test]
fn check_reports_available_current_and_older() -> TestResult {
    let fetch = FakeFetch::release();
    for (installed, want) in [
        ("1.2.2", Status::Available),
        ("0.0.1", Status::Available),
        ("1.2.3", Status::UpToDate),
        ("1.2.4", Status::Older),
        ("2.0.0", Status::Older),
    ] {
        let checked = check(&fetch, &settings(installed)?)?;
        assert_eq!(checked.status, want, "installed {installed}");
        assert_eq!(checked.release.version.to_string(), "1.2.3");
    }
    assert!(!fetch.fetched_archive(), "check must not download archives");
    Ok(())
}

#[test]
fn settings_fail_closed() {
    let key = || keys();
    assert_eq!(
        Settings::new(BASE, keys::embedded_keys(), "x86_64", "0.0.1").err(),
        Some(UpdateError::Keys(KeyError::Placeholder)),
        "the shipped placeholder key must stop updates"
    );
    assert_eq!(
        Settings::new("http://updates.example.test/x", key(), "x86_64", "0.0.1").err(),
        Some(UpdateError::NotHttps)
    );
    assert_eq!(
        Settings::new(
            "https://github.com/OWNER/craft-a-server/releases/latest/download",
            key(),
            "x86_64",
            "0.0.1"
        )
        .err(),
        Some(UpdateError::NotConfigured),
        "a leftover placeholder owner must stop updates"
    );
    assert!(
        Settings::new(
            craft_a_server_core::config::DEFAULT_UPDATE_URL,
            key(),
            "x86_64",
            "0.0.1"
        )
        .is_ok()
    );
    assert_eq!(
        Settings::new(BASE, key(), "riscv64", "0.0.1").err(),
        Some(UpdateError::UnsupportedPlatform("riscv64".into()))
    );
    assert!(Settings::new(BASE, key(), "x86_64", "banana").is_err());
    assert!(Settings::new(BASE, key(), "aarch64", "0.0.1").is_ok());
}

#[test]
fn trailing_slash_in_the_base_url_is_harmless() -> TestResult {
    let s = Settings::new(&format!("{BASE}/"), keys(), "x86_64", "1.0.0")?;
    assert_eq!(s.url("SHA256SUMS"), format!("{BASE}/SHA256SUMS"));
    assert_eq!(s.host(), "updates.example.test");
    Ok(())
}

#[test]
fn an_unsigned_list_is_refused() -> TestResult {
    let mut fetch = FakeFetch::release();
    fetch.routes.remove(&format!("{BASE}/SHA256SUMS.minisig"));
    let err = check(&fetch, &settings("1.0.0")?).err();
    assert!(
        matches!(
            err,
            Some(UpdateError::Download {
                what: "SHA256SUMS.minisig",
                ..
            })
        ),
        "{err:?}"
    );
    Ok(())
}

#[test]
fn a_list_signed_by_another_key_is_refused() -> TestResult {
    let mut fetch = FakeFetch::release();
    fetch.put("SHA256SUMS.minisig", SIG_B.as_bytes());
    assert_eq!(
        check(&fetch, &settings("1.0.0")?).err(),
        Some(UpdateError::Verify(VerifyError::SignatureMismatch))
    );
    Ok(())
}

#[test]
fn a_tampered_list_is_refused_even_if_it_still_parses() -> TestResult {
    let mut fetch = FakeFetch::release();
    // Swap the archive hash for another valid-looking one, as an attacker who
    // controls the download host would.
    let first8 = SUMS.get(..8).ok_or("short")?;
    let evil = SUMS.replacen(first8, "deadbeef", 1);
    assert_ne!(evil, SUMS);
    fetch.put("SHA256SUMS", evil.as_bytes());
    assert_eq!(
        check(&fetch, &settings("1.0.0")?).err(),
        Some(UpdateError::Verify(VerifyError::SignatureMismatch))
    );
    Ok(())
}

#[test]
fn garbage_signature_text_is_refused() -> TestResult {
    let mut fetch = FakeFetch::release();
    fetch.put("SHA256SUMS.minisig", &[0xff, 0xfe, 0x00]);
    assert!(check(&fetch, &settings("1.0.0")?).is_err());
    fetch.put("SHA256SUMS.minisig", b"not a signature");
    assert_eq!(
        check(&fetch, &settings("1.0.0")?).err(),
        Some(UpdateError::Verify(VerifyError::BadSignatureFormat))
    );
    Ok(())
}

#[test]
fn apply_stages_verified_files_and_calls_the_installer() -> TestResult {
    let fetch = FakeFetch::release();
    let env = env()?;
    let runner = FakeRunner::default();
    let mut asked = 0;
    let out = apply(&fetch, &settings("1.0.0")?, &env.paths, &runner, &mut |c| {
        asked += 1;
        assert_eq!(c.release.version.to_string(), "1.2.3");
        true
    })?;
    assert_eq!(out, Applied::Installed(Version::parse("1.2.3").ok_or("v")?));
    assert_eq!(asked, 1);

    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 1);
    let (program, args, files) = calls.first().ok_or("no call")?;
    assert_eq!(program, &env.paths.installer);
    assert_eq!(
        args,
        &[
            OsString::from("--upgrade"),
            OsString::from("--release-dir"),
            env.paths.staging.clone().into_os_string(),
            OsString::from("--yes"),
        ]
    );
    assert_eq!(
        files,
        &["SHA256SUMS", "SHA256SUMS.minisig", ARCHIVE_NAME].map(String::from)
    );
    assert!(!env.paths.staging.exists(), "staging is cleaned afterwards");
    Ok(())
}

#[test]
fn a_bad_archive_is_refused_before_anything_is_written() -> TestResult {
    let mut fetch = FakeFetch::release();
    fetch.put(ARCHIVE_NAME, b"swapped by someone");
    let env = env()?;
    let runner = FakeRunner::default();
    assert_eq!(
        apply(&fetch, &settings("1.0.0")?, &env.paths, &runner, &mut yes).err(),
        Some(UpdateError::ArchiveHash)
    );
    assert!(runner.calls.borrow().is_empty());
    assert!(!env.paths.staging.exists());
    Ok(())
}

#[test]
fn an_oversize_archive_is_refused() -> TestResult {
    struct Huge(FakeFetch);
    impl Fetch for Huge {
        fn get(&self, url: &str, max: u64) -> Result<Vec<u8>, FetchError> {
            if url.ends_with(".tar.gz") {
                return Err(FetchError::TooLarge(max));
            }
            self.0.get(url, max)
        }
    }
    let env = env()?;
    let runner = FakeRunner::default();
    let err = apply(
        &Huge(FakeFetch::release()),
        &settings("1.0.0")?,
        &env.paths,
        &runner,
        &mut yes,
    )
    .err();
    assert!(matches!(err, Some(UpdateError::Download { .. })), "{err:?}");
    assert!(runner.calls.borrow().is_empty());
    Ok(())
}

#[test]
fn downgrades_are_refused_and_nothing_is_downloaded() -> TestResult {
    let fetch = FakeFetch::release();
    let env = env()?;
    let runner = FakeRunner::default();
    let err = apply(&fetch, &settings("2.0.0")?, &env.paths, &runner, &mut yes).err();
    assert_eq!(
        err,
        Some(UpdateError::Downgrade {
            installed: Version::parse("2.0.0").ok_or("v")?,
            offered: Version::parse("1.2.3").ok_or("v")?,
        })
    );
    assert!(!fetch.fetched_archive());
    assert!(runner.calls.borrow().is_empty());
    Ok(())
}

#[test]
fn the_same_version_is_not_reinstalled() -> TestResult {
    let fetch = FakeFetch::release();
    let env = env()?;
    let runner = FakeRunner::default();
    let out = apply(&fetch, &settings("1.2.3")?, &env.paths, &runner, &mut yes)?;
    assert_eq!(
        out,
        Applied::AlreadyCurrent(Version::parse("1.2.3").ok_or("v")?)
    );
    assert!(!fetch.fetched_archive());
    assert!(runner.calls.borrow().is_empty());
    Ok(())
}

#[test]
fn declining_leaves_everything_untouched() -> TestResult {
    let fetch = FakeFetch::release();
    let env = env()?;
    let runner = FakeRunner::default();
    let out = apply(
        &fetch,
        &settings("1.0.0")?,
        &env.paths,
        &runner,
        &mut |_| false,
    )?;
    assert_eq!(out, Applied::Declined);
    assert!(!fetch.fetched_archive());
    assert!(!env.paths.staging.exists());
    assert!(runner.calls.borrow().is_empty());
    Ok(())
}

#[test]
fn a_failing_installer_is_reported_and_staging_removed() -> TestResult {
    let fetch = FakeFetch::release();
    let env = env()?;
    let runner = FakeRunner {
        fail_with: Some("exit status: 3".into()),
        ..FakeRunner::default()
    };
    assert_eq!(
        apply(&fetch, &settings("1.0.0")?, &env.paths, &runner, &mut yes).err(),
        Some(UpdateError::InstallerFailed("exit status: 3".into()))
    );
    assert!(!env.paths.staging.exists());
    Ok(())
}

#[test]
fn a_missing_installer_stops_before_downloading_the_archive() -> TestResult {
    let fetch = FakeFetch::release();
    let mut env = env()?;
    env.paths.installer = env.paths.staging.join("nope.sh");
    let runner = FakeRunner::default();
    let err = apply(&fetch, &settings("1.0.0")?, &env.paths, &runner, &mut yes).err();
    assert!(
        matches!(err, Some(UpdateError::InstallerMissing(_))),
        "{err:?}"
    );
    assert!(!fetch.fetched_archive());
    Ok(())
}

#[test]
fn a_leftover_staging_directory_is_replaced() -> TestResult {
    let fetch = FakeFetch::release();
    let env = env()?;
    std::fs::create_dir_all(&env.paths.staging)?;
    std::fs::write(env.paths.staging.join("stale"), "old")?;
    let runner = FakeRunner::default();
    apply(&fetch, &settings("1.0.0")?, &env.paths, &runner, &mut yes)?;
    let calls = runner.calls.borrow();
    let (_, _, files) = calls.first().ok_or("no call")?;
    assert!(!files.iter().any(|f| f == "stale"));
    Ok(())
}

#[test]
fn the_cli_and_the_installer_embed_the_same_keys() -> TestResult {
    let script = include_str!("../../../../deploy/install.sh");
    let begin = "# BEGIN EMBEDDED RELEASE KEYS";
    let end = "# END EMBEDDED RELEASE KEYS";
    let start = script.find(begin).ok_or("begin marker")? + begin.len();
    let stop = script.find(end).ok_or("end marker")?;
    let block = script.get(start..stop).ok_or("block")?;
    // The script keeps the lines inside RELEASE_KEYS='...'.
    let in_script: Vec<&str> = block
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty() && !l.starts_with('#') && !l.starts_with("RELEASE_KEYS=") && *l != "'"
        })
        .collect();
    assert_eq!(in_script, keys::key_lines(keys::EMBEDDED));
    Ok(())
}
