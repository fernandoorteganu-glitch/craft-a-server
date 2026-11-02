//! Signature and checksum checks. Pure functions, no I/O.
//!
//! Trust chain: a minisign signature over `SHA256SUMS`, checked against the
//! embedded public keys, authenticates the list; the list authenticates each
//! release archive by SHA-256. Nothing in a response is believed before both
//! checks pass.

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

use super::version::Version;

/// Prefix of every release archive name.
const ASSET_PREFIX: &str = "craft-a-server-";
const ASSET_SUFFIX: &str = ".tar.gz";
const MAX_SUM_LINES: usize = 64;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("the signature file is malformed")]
    BadSignatureFormat,
    #[error("the signature does not match any trusted release key")]
    SignatureMismatch,
    #[error("SHA256SUMS is malformed: {0}")]
    BadSums(&'static str),
    #[error("SHA256SUMS lists no release for {0}")]
    NoRelease(String),
    #[error("SHA256SUMS lists more than one release for {0}")]
    AmbiguousRelease(String),
}

/// Verifies `message` against a minisign signature file's text. Accepts the
/// signature if any key verifies it. Legacy (non-prehashed) signatures are
/// refused: `minisign -S` has produced prehashed ones by default for years,
/// and the legacy form signs the data directly, which we never need.
pub fn verify_signature(
    keys: &[PublicKey],
    message: &[u8],
    signature_text: &str,
) -> Result<(), VerifyError> {
    let signature =
        Signature::decode(signature_text).map_err(|_| VerifyError::BadSignatureFormat)?;
    if keys
        .iter()
        .any(|k| k.verify(message, &signature, false).is_ok())
    {
        Ok(())
    } else {
        Err(VerifyError::SignatureMismatch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumEntry {
    /// Lower-case hex SHA-256.
    pub sha256: String,
    pub name: String,
}

/// Lower-case hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Whether `data` hashes to `expected_hex` (case-insensitive).
pub fn hash_matches(data: &[u8], expected_hex: &str) -> bool {
    sha256_hex(data).eq_ignore_ascii_case(expected_hex)
}

/// A file name we are willing to see in a signed list: letters, digits, `.`,
/// `_`, `-`; no leading dot; so it can never be a path.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Parses `sha256sum` output: `<64 hex>  <name>` (a `*` binary marker before
/// the name is tolerated). Blank lines are skipped.
pub fn parse_sums(text: &str) -> Result<Vec<SumEntry>, VerifyError> {
    let mut out: Vec<SumEntry> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if out.len() >= MAX_SUM_LINES {
            return Err(VerifyError::BadSums("too many lines"));
        }
        let (hash, rest) = line
            .split_once(' ')
            .ok_or(VerifyError::BadSums("a line has no file name"))?;
        let name = rest
            .strip_prefix(' ')
            .or_else(|| rest.strip_prefix('*'))
            .ok_or(VerifyError::BadSums("a line is not in sha256sum format"))?;
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(VerifyError::BadSums("a hash is not 64 hex digits"));
        }
        if !valid_name(name) {
            return Err(VerifyError::BadSums("a file name is not allowed"));
        }
        if out.iter().any(|e| e.name == name) {
            return Err(VerifyError::BadSums("a file name is listed twice"));
        }
        out.push(SumEntry {
            sha256: hash.to_ascii_lowercase(),
            name: name.to_owned(),
        });
    }
    if out.is_empty() {
        return Err(VerifyError::BadSums("the list is empty"));
    }
    Ok(out)
}

/// One installable release archive named by a verified list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub name: String,
    pub sha256: String,
}

/// The archive name for a version and target triple.
#[cfg(test)]
pub fn asset_name(version: Version, triple: &str) -> String {
    format!("{ASSET_PREFIX}{version}-{triple}{ASSET_SUFFIX}")
}

/// Finds the single `craft-a-server-<version>-<triple>.tar.gz` entry.
pub fn pick_release(entries: &[SumEntry], triple: &str) -> Result<Release, VerifyError> {
    let tail = format!("-{triple}{ASSET_SUFFIX}");
    let mut found: Vec<Release> = entries
        .iter()
        .filter_map(|e| {
            let version = e
                .name
                .strip_prefix(ASSET_PREFIX)?
                .strip_suffix(tail.as_str())?;
            Some(Release {
                version: Version::parse(version)?,
                name: e.name.clone(),
                sha256: e.sha256.clone(),
            })
        })
        .collect();
    match found.len() {
        0 => Err(VerifyError::NoRelease(triple.to_owned())),
        1 => found
            .pop()
            .ok_or_else(|| VerifyError::NoRelease(triple.to_owned())),
        _ => Err(VerifyError::AmbiguousRelease(triple.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &str = include_str!("../../tests/fixtures/update/SHA256SUMS");
    const SIG: &str = include_str!("../../tests/fixtures/update/SHA256SUMS.minisig");
    const SIG_B: &str = include_str!("../../tests/fixtures/update/SHA256SUMS.keyb.minisig");
    const KEY_A: &str = include_str!("../../tests/fixtures/update/key_a.pub");
    const KEY_B: &str = include_str!("../../tests/fixtures/update/key_b.pub");
    const X86: &str = "x86_64-unknown-linux-gnu";

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn key(text: &str) -> Result<PublicKey, minisign_verify::Error> {
        PublicKey::from_base64(text.trim())
    }

    #[test]
    fn a_real_minisign_signature_verifies() -> TestResult {
        verify_signature(&[key(KEY_A)?], SUMS.as_bytes(), SIG)?;
        Ok(())
    }

    #[test]
    fn any_of_the_listed_keys_may_have_signed() -> TestResult {
        let keys = [key(KEY_B)?, key(KEY_A)?];
        verify_signature(&keys, SUMS.as_bytes(), SIG)?;
        verify_signature(&keys, SUMS.as_bytes(), SIG_B)?;
        Ok(())
    }

    #[test]
    fn a_signature_from_an_untrusted_key_is_refused() -> TestResult {
        assert_eq!(
            verify_signature(&[key(KEY_A)?], SUMS.as_bytes(), SIG_B),
            Err(VerifyError::SignatureMismatch)
        );
        Ok(())
    }

    #[test]
    fn any_change_to_the_signed_text_breaks_the_signature() -> TestResult {
        let keys = [key(KEY_A)?];
        let mut tampered = SUMS.to_owned();
        tampered.push('\n');
        assert_eq!(
            verify_signature(&keys, tampered.as_bytes(), SIG),
            Err(VerifyError::SignatureMismatch)
        );
        let swapped = SUMS.replacen('2', "3", 1);
        assert_ne!(swapped, SUMS);
        assert_eq!(
            verify_signature(&keys, swapped.as_bytes(), SIG),
            Err(VerifyError::SignatureMismatch)
        );
        assert_eq!(
            verify_signature(&keys, b"", SIG),
            Err(VerifyError::SignatureMismatch)
        );
        Ok(())
    }

    #[test]
    fn a_changed_trusted_comment_breaks_the_signature() -> TestResult {
        let edited = SIG.replace("fixture 1.2.3", "fixture 9.9.9");
        assert_ne!(edited, SIG);
        assert!(verify_signature(&[key(KEY_A)?], SUMS.as_bytes(), &edited).is_err());
        Ok(())
    }

    #[test]
    fn malformed_signatures_and_no_keys_are_refused() -> TestResult {
        let keys = [key(KEY_A)?];
        for bad in ["", "garbage", "untrusted comment: x\n!!!\n"] {
            assert_eq!(
                verify_signature(&keys, SUMS.as_bytes(), bad),
                Err(VerifyError::BadSignatureFormat),
                "{bad:?}"
            );
        }
        assert_eq!(
            verify_signature(&[], SUMS.as_bytes(), SIG),
            Err(VerifyError::SignatureMismatch)
        );
        Ok(())
    }

    #[test]
    fn fixture_list_parses_and_picks_the_right_archive() -> TestResult {
        let entries = parse_sums(SUMS)?;
        assert_eq!(entries.len(), 2);
        let x86 = pick_release(&entries, X86)?;
        assert_eq!(x86.version.to_string(), "1.2.3");
        assert_eq!(
            x86.name,
            "craft-a-server-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        );
        let arm = pick_release(&entries, "aarch64-unknown-linux-gnu")?;
        assert_ne!(arm.sha256, x86.sha256);
        assert_eq!(
            pick_release(&entries, "riscv64-unknown-linux-gnu"),
            Err(VerifyError::NoRelease("riscv64-unknown-linux-gnu".into()))
        );
        Ok(())
    }

    fn line(hash_char: char, name: &str) -> String {
        format!("{}  {name}\n", hash_char.to_string().repeat(64))
    }

    #[test]
    fn two_versions_for_one_target_is_ambiguous() -> TestResult {
        let text = format!(
            "{}{}",
            line('a', "craft-a-server-1.0.0-x86_64-unknown-linux-gnu.tar.gz"),
            line('b', "craft-a-server-1.0.1-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            pick_release(&parse_sums(&text)?, X86),
            Err(VerifyError::AmbiguousRelease(X86.into()))
        );
        Ok(())
    }

    #[test]
    fn lookalike_names_are_not_releases() -> TestResult {
        let text = format!(
            "{}{}{}{}",
            line('a', "craft-a-server-1.0-x86_64-unknown-linux-gnu.tar.gz"),
            line(
                'a',
                "craft-a-server-1.0.0-rc1-x86_64-unknown-linux-gnu.tar.gz"
            ),
            line('a', "other-1.0.0-x86_64-unknown-linux-gnu.tar.gz"),
            line(
                'a',
                "craft-a-server-1.0.0-x86_64-unknown-linux-gnu.tar.gz.sig"
            ),
        );
        assert_eq!(
            pick_release(&parse_sums(&text)?, X86),
            Err(VerifyError::NoRelease(X86.into()))
        );
        Ok(())
    }

    #[test]
    fn sums_parser_rejects_bad_lines() {
        let h = "a".repeat(64);
        let long = "n".repeat(101);
        for (text, why) in [
            (String::new(), "empty"),
            ("\n\n".to_owned(), "blank"),
            (format!("{h} name\n"), "one space"),
            (format!("{h}\tname\n"), "tab"),
            (format!("{} name\n", "a".repeat(63)), "short hash"),
            (format!("{}  name\n", "g".repeat(64)), "non hex"),
            (format!("{h}  ../evil\n"), "traversal"),
            (format!("{h}  dir/file\n"), "slash"),
            (format!("{h}  .hidden\n"), "dot file"),
            (format!("{h}  a..b\n"), "double dot"),
            (format!("{h}  \n"), "no name"),
            (format!("{h}  {long}\n"), "long name"),
            (format!("{h}  a b\n"), "space in name"),
            (format!("{h}  a\n{h}  a\n"), "duplicate"),
            (h.clone(), "no name at all"),
        ] {
            assert!(parse_sums(&text).is_err(), "{why}");
        }
        let many: String = (0..65).map(|i| line('a', &format!("f{i}"))).collect();
        assert!(parse_sums(&many).is_err());
    }

    #[test]
    fn sums_parser_accepts_binary_marker_and_upper_case() -> TestResult {
        let text = format!("{}  a\n{} *b\n", "A".repeat(64), "b".repeat(64));
        let entries = parse_sums(&text)?;
        assert_eq!(
            entries.first().map(|e| e.sha256.clone()),
            Some("a".repeat(64))
        );
        assert_eq!(entries.get(1).map(|e| e.name.as_str()), Some("b"));
        Ok(())
    }

    #[test]
    fn hashing_matches_the_fixture_archive() -> TestResult {
        let archive = include_bytes!(
            "../../tests/fixtures/update/craft-a-server-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
        );
        let release = pick_release(&parse_sums(SUMS)?, X86)?;
        assert!(hash_matches(archive, &release.sha256));
        assert!(!hash_matches(b"other", &release.sha256));
        assert!(!hash_matches(archive, ""));
        Ok(())
    }

    #[test]
    fn asset_names_round_trip() -> TestResult {
        let v = Version::parse("3.4.5").ok_or("version")?;
        let entries = [SumEntry {
            sha256: "0".repeat(64),
            name: asset_name(v, X86),
        }];
        assert_eq!(pick_release(&entries, X86)?.version, v);
        Ok(())
    }
}
