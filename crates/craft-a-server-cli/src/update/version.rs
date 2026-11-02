//! Release versions: strict `MAJOR.MINOR.PATCH`, compared numerically.

use std::fmt;

/// A plain three-part version. Pre-release and build suffixes are not
/// accepted: the release pipeline only publishes final versions, and anything
/// else in a signed file is treated as corrupt rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parses `1.2.3`. Rejects empty parts, signs, spaces, leading zeros
    /// (`01`), more or fewer than three parts, and parts over 9 digits.
    pub fn parse(text: &str) -> Option<Version> {
        let mut parts = text.split('.');
        let major = part(parts.next()?)?;
        let minor = part(parts.next()?)?;
        let patch = part(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

fn part(text: &str) -> Option<u32> {
    let digits_only = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if !digits_only || text.len() > 9 || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints_round_trip() {
        for text in ["0.0.1", "1.2.3", "10.20.30", "0.0.0", "999999999.0.1"] {
            assert_eq!(
                Version::parse(text).map(|v| v.to_string()).as_deref(),
                Some(text)
            );
        }
    }

    #[test]
    fn rejects_everything_that_is_not_plain() {
        for text in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "a.b.c",
            "1.2.x",
            "1..3",
            " 1.2.3",
            "1.2.3 ",
            "+1.2.3",
            "-1.2.3",
            "1.2.3-rc1",
            "1.2.3+b",
            "1_0.2.3",
            "1234567890.0.0",
            "v1.2.3",
        ] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn compares_numerically_not_textually() -> Result<(), String> {
        let v = |s: &str| Version::parse(s).ok_or_else(|| format!("bad {s}"));
        assert!(v("1.10.0")? > v("1.9.9")?);
        assert!(v("2.0.0")? > v("1.99.99")?);
        assert!(v("0.0.2")? > v("0.0.1")?);
        assert_eq!(v("1.2.3")?, v("1.2.3")?);
        Ok(())
    }
}
