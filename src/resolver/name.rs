//! `Name` — the one identifier shape fghj accepts, checked at deserialize time.
//! CUE is only the author's lint; serde is the real boundary.
//! `rust_and_cue_agree_on_what_a_name_is` keeps the two in step.

use std::fmt;

use serde::{Deserialize, Deserializer};

/// Kept as a string for the drift test; `why_invalid` is the actual check.
pub const NAME_PATTERN: &str = "^[a-z0-9][a-z0-9-]*$";

/// The intersection of DNS label, network alias and container name, so nothing
/// downstream has to escape.
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct Name(String);

impl Name {
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
            assert!(why_invalid(ok).is_none(), "{ok} should be valid");
        }
    }

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
            assert!(why_invalid(bad).is_some(), "{bad:?} should be invalid");
        }
    }

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

    #[test]
    fn rust_and_cue_agree_on_what_a_name_is() {
        // Every `=~"..."` in the schemas, minus non-name ones (image refs, env vars,
        // host aliases, port keys, git URLs, domains).
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
        // Guards against the filter matching nothing.
        assert!(
            found >= 6,
            "expected the schemas to constrain names, found {found}"
        );
    }
}
