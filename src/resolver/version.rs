//! `version:` — major is a compatibility barrier, minor is not, so peer repos
//! can adopt a newer minor without a flag day. A newer minor is accepted with
//! an advisory warning.

use std::fmt;

use serde::{Deserialize, Deserializer};

/// Bump `minor` for backwards-compatible fields, `major` only when an older
/// file would mean something different.
pub const SCHEMA_VERSION: Version = Version { major: 2, minor: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (major, minor) = s.split_once('.').ok_or_else(|| {
            format!("version '{s}' is not major.minor (expected something like \"2.0\")")
        })?;
        let parse_part = |part: &str, which: &str| {
            part.parse::<u32>()
                .map_err(|_| format!("version '{s}' has a non-numeric {which} version ('{part}')"))
        };
        let version = Version {
            major: parse_part(major, "major")?,
            minor: parse_part(minor, "minor")?,
        };
        if version.major != SCHEMA_VERSION.major {
            return Err(format!(
                "version '{s}' is not supported: this fghj understands {}.x. A different \
                 major version means this file says something this build would read \
                 differently, so it is refused rather than guessed at",
                SCHEMA_VERSION.major
            ));
        }
        Ok(version)
    }

    pub fn is_newer_minor_than_supported(&self) -> bool {
        self.minor > SCHEMA_VERSION.minor
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        Version::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_version_parses() {
        assert_eq!(Version::parse("2.0").unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_minor_is_accepted_and_flagged() {
        let v = Version::parse("2.7").unwrap();
        assert!(v.is_newer_minor_than_supported());
    }

    #[test]
    fn a_known_minor_is_accepted_without_a_flag() {
        assert!(
            !Version::parse("2.0")
                .unwrap()
                .is_newer_minor_than_supported()
        );
    }

    #[test]
    fn a_different_major_is_refused_and_says_what_is_supported() {
        let err = Version::parse("3.0").unwrap_err();
        assert!(err.contains("2.x"), "{err}");
        let err = Version::parse("1.0").unwrap_err();
        assert!(err.contains("not supported"), "{err}");
    }

    #[test]
    fn a_malformed_version_says_what_was_expected() {
        for bad in ["1", "", "one.zero", "1.x", "v1.0"] {
            let err = Version::parse(bad).unwrap_err();
            assert!(
                err.contains("major.minor") || err.contains("non-numeric"),
                "{bad:?} -> {err}"
            );
        }
    }

    /// The schema must not reject a newer minor the daemon would accept.
    #[test]
    fn the_schema_accepts_the_same_majors_this_build_does() {
        let schema = include_str!("../../schema/component.cue");
        let expected = format!("^{}\\\\.[0-9]+$", SCHEMA_VERSION.major);
        assert!(
            schema.contains(&expected),
            "schema/component.cue should constrain version as {expected:?} to match \
             `SCHEMA_VERSION` ({SCHEMA_VERSION})"
        );
    }

    #[test]
    fn deserializing_rejects_an_unsupported_version() {
        let err = serde_yaml::from_str::<Version>("\"3.4\"").unwrap_err();
        assert!(err.to_string().contains("2.x"), "{err}");
    }
}
