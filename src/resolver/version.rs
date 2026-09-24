//! `version:` — what a `.fghj.yaml` claims to be written against.
//!
//! The audit (E8) put it sharply: the CUE pins `version` to the literal
//! `"1.0"`, and the whole premise of the design is that no repo owns the
//! config — every repo is a peer, each ships its own `.fghj.yaml`, and they
//! resolve into one graph. Those two facts are incompatible. There is no
//! way for one repo to adopt a v1.1 feature while its peers are still on
//! v1.0, because they must all resolve together, so any evolution requires
//! every repo in every workspace to change at once — reintroducing exactly
//! the central-coordination problem the federated model exists to abolish.
//!
//! At runtime it was moot in the worst way: serde never checked the value
//! at all, so `version` was decoration.
//!
//! So: **major is a compatibility barrier, minor is not.**
//!
//! - A *different major* is rejected outright. It means the file says
//!   something this build cannot correctly interpret, and guessing at it
//!   would be worse than refusing.
//! - Any *minor* is accepted, including one newer than this build knows.
//!   That is what makes staggered adoption possible: a repo can start using
//!   a 1.1 feature while its peers stay on 1.0, and they still resolve
//!   together.
//!
//! Accepting a newer minor silently would be its own failure, though — the
//! features added after this build's minor are simply not there, and the
//! symptom would be config that reads as if it were honoured. So
//! `scan_workspace` pairs acceptance with an advisory warning naming the
//! repo and both versions.

use std::fmt;

use serde::{Deserialize, Deserializer};

/// The schema version this build implements. Bump `minor` when adding a
/// backwards-compatible field; bump `major` only for a change that would
/// make an older file mean something different.
pub const SCHEMA_VERSION: Version = Version { major: 1, minor: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// `Err` carries a ready-to-show explanation.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (major, minor) = s.split_once('.').ok_or_else(|| {
            format!("version '{s}' is not major.minor (expected something like \"1.0\")")
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

    /// True when the file was written against a newer minor than this build
    /// implements — accepted, but worth saying out loud.
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
        assert_eq!(Version::parse("1.0").unwrap(), SCHEMA_VERSION);
    }

    /// The point of E8: a peer adopting a newer minor must still resolve
    /// alongside repos that have not, with no flag day.
    #[test]
    fn a_newer_minor_is_accepted_and_flagged() {
        let v = Version::parse("1.7").unwrap();
        assert!(v.is_newer_minor_than_supported());
    }

    #[test]
    fn a_known_minor_is_accepted_without_a_flag() {
        assert!(
            !Version::parse("1.0")
                .unwrap()
                .is_newer_minor_than_supported()
        );
    }

    /// Major is the barrier: refusing beats guessing, since the same file
    /// would mean something different.
    #[test]
    fn a_different_major_is_refused_and_says_what_is_supported() {
        let err = Version::parse("2.0").unwrap_err();
        assert!(err.contains("1.x"), "{err}");
        let err = Version::parse("0.9").unwrap_err();
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

    /// Same contract as `name::rust_and_cue_agree_on_what_a_name_is`: the
    /// schema is the author's self-check, so it must not reject a file the
    /// daemon would accept. A literal `"1.0"` there — which is what it used
    /// to say — would fail `fghj validate` on a repo that had legitimately
    /// moved to 1.1, reintroducing the flag day this split exists to
    /// remove.
    #[test]
    fn the_schema_accepts_the_same_majors_this_build_does() {
        let schema = include_str!("../../schema/component.cue");
        let expected = format!("^{}\\\\.[0-9]+$", SCHEMA_VERSION.major);
        assert!(
            schema.contains(&expected),
            "schema/component.cue should constrain version as {expected:?} to match              `SCHEMA_VERSION` ({SCHEMA_VERSION})"
        );
    }

    /// `version` used to be an unchecked `String` — decoration. It is now
    /// the one field a component cannot get wrong quietly.
    #[test]
    fn deserializing_rejects_an_unsupported_version() {
        let err = serde_yaml::from_str::<Version>("\"3.4\"").unwrap_err();
        assert!(err.to_string().contains("1.x"), "{err}");
    }
}
