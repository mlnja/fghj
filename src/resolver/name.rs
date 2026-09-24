//! `Name` — the one identifier shape fghj accepts, enforced where the
//! bytes actually enter the process.
//!
//! The audit's framing of this (B8) was that `schema/*.cue` is a linter,
//! not a type system: it is enforced only by `fghj validate`, an opt-in CLI
//! command shelling out to an external `cue` binary that may not be
//! installed, and which the daemon never invokes. The real runtime boundary
//! is serde, and serde was strictly more permissive. A service named
//! `"My Service!"` parsed fine, flowed unsanitized into `node.id`, and from
//! there into a DNS name and a Docker network alias that are simply invalid
//! — while the *container* name, which goes through `sanitize_label`,
//! worked. Three namespaces, three different escaping disciplines, one
//! unvalidated input.
//!
//! So the constraint lives here, in the type, and `Deserialize` is where it
//! is checked. CUE remains a convenience for the person writing the file
//! (and for their editor, agent, or CI); it is not what the daemon trusts.
//! What CUE must *not* do is tell an author something is fine that fghj
//! will then reject — see `rust_and_cue_agree_on_what_a_name_is`, which
//! reads the schemas and fails if they ever disagree.

use std::fmt;

use serde::{Deserialize, Deserializer};

/// The literal the schemas spell as `=~"^[a-z0-9][a-z0-9-]*$"`. Kept as a
/// string so the drift test can compare it against the schema text
/// verbatim; the actual check is [`is_valid`], hand-written because the
/// pattern is small and a bespoke check can say *why* a name was rejected,
/// which a regex mismatch cannot.
pub const NAME_PATTERN: &str = "^[a-z0-9][a-z0-9-]*$";

/// Lowercase alphanumerics and dashes, starting with an alphanumeric.
///
/// This is deliberately the intersection of what every downstream
/// namespace accepts rather than the union: a DNS label, a Docker network
/// alias, and a container name all tolerate it unchanged, so nothing
/// downstream has to escape and no two escapings can disagree.
pub fn is_valid(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Explains the *first* thing wrong with `s`, in the words an author can
/// act on. Returns `None` when `s` is a valid name.
fn why_invalid(s: &str) -> Option<String> {
    if s.is_empty() {
        return Some("a name cannot be empty".into());
    }
    if let Some(c) = s.chars().find(|c| c.is_ascii_uppercase()) {
        return Some(format!(
            "'{c}' is uppercase; names are lowercase because they become DNS labels, \
             which are case-insensitive"
        ));
    }
    if let Some(c) = s
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Some(format!(
            "'{c}' is not allowed; names may contain only lowercase letters, digits \
             and dashes"
        ));
    }
    if s.starts_with('-') {
        return Some("a name cannot start with a dash".into());
    }
    None
}

/// A validated identifier. Constructible only by deserializing or via
/// [`Name::parse`], so holding one is proof it is safe to put in a domain,
/// a network alias and a container name without further escaping.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct Name(String);

impl Name {
    /// `Err` carries a ready-to-show explanation, not just a bool.
    pub fn parse(s: impl Into<String>) -> Result<Self, String> {
        let s = s.into();
        match why_invalid(&s) {
            Some(why) => Err(format!("invalid name '{s}': {why}")),
            None => Ok(Name(s)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        Name::parse(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_parse() {
        for ok in ["api", "shop-web", "s3", "a", "0", "x-1-y"] {
            assert!(Name::parse(ok).is_ok(), "{ok} should be valid");
            assert!(is_valid(ok), "{ok} should be valid");
        }
    }

    /// The exact example B8 names.
    #[test]
    fn the_name_that_used_to_reach_dns_unsanitized_is_rejected() {
        let err = Name::parse("My Service!").unwrap_err();
        assert!(err.contains("My Service!"), "{err}");
        assert!(err.contains("uppercase"), "{err}");
    }

    #[test]
    fn every_rejection_says_why() {
        for (bad, expected) in [
            ("", "empty"),
            ("-leading", "cannot start with a dash"),
            ("has space", "not allowed"),
            ("under_score", "not allowed"),
            ("has.dot", "not allowed"),
            ("MiXeD", "uppercase"),
        ] {
            let err = Name::parse(bad).unwrap_err();
            assert!(err.contains(expected), "{bad:?} -> {err}");
            assert!(!is_valid(bad), "{bad:?} should be invalid");
        }
    }

    /// A dash may not *start* a name (it would make an invalid DNS label)
    /// but is fine anywhere else, including at the end.
    #[test]
    fn a_leading_dash_is_the_only_positional_rule() {
        assert!(Name::parse("-x").is_err());
        assert!(Name::parse("x-").is_ok());
        assert!(Name::parse("x--y").is_ok());
    }

    #[test]
    fn deserializing_rejects_an_invalid_name_with_a_useful_message() {
        let err = serde_yaml::from_str::<Name>("My Service!").unwrap_err();
        assert!(err.to_string().contains("uppercase"), "{err}");
    }

    /// CUE is the author's self-check, not what the daemon trusts — but it
    /// must never tell an author their file is fine when fghj will reject
    /// it. Every identifier constraint in the schemas therefore has to be
    /// exactly the pattern this module implements; a schema that is *more
    /// permissive* would lie to the person running `fghj validate`.
    ///
    /// Checked by reading the schema text rather than by convention,
    /// because a comment saying "keep these in sync" is precisely the thing
    /// that does not stay in sync.
    #[test]
    fn rust_and_cue_agree_on_what_a_name_is() {
        // Every `=~"..."` in the schemas, with the ones that constrain
        // things that are not names (image refs, env vars, host aliases,
        // numeric port keys, git URLs, dotted domains) filtered out by
        // their own distinctive syntax.
        let schemas = [
            include_str!("../../schema/component.cue"),
            include_str!("../../schema/dependency.cue"),
        ];
        let mut found = 0;
        for text in schemas {
            for (idx, _) in text.match_indices("=~\"") {
                let rest = &text[idx + 3..];
                let end = rest.find('"').expect("unterminated =~ pattern in schema");
                let pattern = &rest[..end];
                // Only the identifier constraints are this module's
                // business; the others describe different codomains.
                if !pattern.starts_with("^[a-z0-9][a-z0-9-]") {
                    continue;
                }
                assert_eq!(
                    pattern, NAME_PATTERN,
                    "a schema constrains a name as {pattern:?}, but fghj enforces \
                     {NAME_PATTERN:?}; `fghj validate` would disagree with the daemon"
                );
                found += 1;
            }
        }
        // Guards against the filter silently matching nothing and the
        // assertion above never running.
        assert!(
            found >= 7,
            "expected the schemas to constrain names, found {found}"
        );
    }
}
