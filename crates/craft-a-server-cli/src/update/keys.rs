//! The release-signing public keys this build trusts.

use minisign_verify::PublicKey;

/// `deploy/release-pubkey.pub`, compiled in. `install.sh` carries the same
/// lines; a test compares them.
pub const EMBEDDED: &str = include_str!("../../../../deploy/release-pubkey.pub");

/// Most keys a key file may list (current plus one rotation successor).
pub const MAX_KEYS: usize = 2;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error(
        "this build carries the placeholder release key, so no update can be trusted \
         (the maintainer has not published a signing key yet)"
    )]
    Placeholder,
    #[error("a release key line is not a valid minisign public key")]
    Invalid,
    #[error("no release key is listed")]
    Empty,
    #[error("more than {MAX_KEYS} release keys are listed")]
    TooMany,
}

/// The key lines of a key file: everything that is not blank or a `#` comment.
pub fn key_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// Parses a key file. Any placeholder line fails the whole file, so a half
/// edited file cannot silently trust fewer or different keys than intended.
pub fn parse_keys(text: &str) -> Result<Vec<PublicKey>, KeyError> {
    let lines = key_lines(text);
    if lines.is_empty() {
        return Err(KeyError::Empty);
    }
    if lines.len() > MAX_KEYS {
        return Err(KeyError::TooMany);
    }
    if lines.iter().any(|l| l.starts_with("PLACEHOLDER")) {
        return Err(KeyError::Placeholder);
    }
    lines
        .iter()
        .map(|l| PublicKey::from_base64(l).map_err(|_| KeyError::Invalid))
        .collect()
}

/// The keys compiled into this binary.
pub fn embedded_keys() -> Result<Vec<PublicKey>, KeyError> {
    parse_keys(EMBEDDED)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE_KEY: &str = "RWQMea9ufMd0zqv+KCL7vaMIHtVE9VxtLwi1AZDCYPFEx5Rvu/aD0VGm";
    const KEY_A: &str = "RWSQsT7Om6zhNSxkirwHBEICQ+idTTZE2c7HeaM+olftyzoQooU2KdQx";

    #[test]
    fn placeholder_fails_closed() {
        assert_eq!(
            parse_keys("# c\nPLACEHOLDER-NOT-A-KEY\n").err(),
            Some(KeyError::Placeholder)
        );
        // A placeholder next to a real key still fails the whole file.
        assert_eq!(
            parse_keys(&format!("{KEY_A}\nPLACEHOLDER-x\n")).err(),
            Some(KeyError::Placeholder)
        );
    }

    #[test]
    fn the_shipped_file_holds_exactly_the_published_release_key() {
        assert_eq!(embedded_keys().map(|k| k.len()), Ok(1));
        assert_eq!(key_lines(EMBEDDED), [RELEASE_KEY]);
    }

    #[test]
    fn accepts_comments_blank_lines_and_up_to_two_keys() {
        assert_eq!(
            parse_keys(&format!("# c\n\n{KEY_A}\n")).map(|k| k.len()),
            Ok(1)
        );
        assert_eq!(
            parse_keys(&format!("{KEY_A}\n{KEY_A}\n")).map(|k| k.len()),
            Ok(2)
        );
    }

    #[test]
    fn rejects_empty_garbage_and_too_many() {
        assert_eq!(
            parse_keys("# only a comment\n").err(),
            Some(KeyError::Empty)
        );
        assert_eq!(parse_keys("not base64 !!\n").err(), Some(KeyError::Invalid));
        assert_eq!(parse_keys("RWQ\n").err(), Some(KeyError::Invalid));
        assert_eq!(
            parse_keys(&format!("{KEY_A}\n{KEY_A}\n{KEY_A}\n")).err(),
            Some(KeyError::TooMany)
        );
    }
}
