//! What a resolver finding is worth, and whether a run may start with it.
//!
//! Every check in the resolver used to push a bare `String` into one flat
//! list whose only consumer was a UI banner. Nothing on the start path read
//! it — `start`, `ensure_running`, `restart_container` and `perform_create`
//! all take a `&Graph` and none of them ever looked at `.warnings` — so a
//! workspace with a dangling `shared-backing` reference and two nodes
//! claiming one domain started happily and misbehaved at runtime, with the
//! explanation sitting unread on a different screen.
//!
//! The fix is not "make every warning fatal". Most are genuinely advisory
//! (a `wildcard` port with no domain to wildcard changes nothing about
//! whether the run works). The distinction that matters is whether the
//! author's config can be carried out *as written*:
//!
//! - [`Severity::Blocking`] — it can't. Something was declared that cannot
//!   be honoured: a dependency that doesn't resolve, two nodes claiming one
//!   name. Starting anyway produces a run that is quietly not the one the
//!   config describes, which is worse than not starting.
//! - [`Severity::Advisory`] — it can, and something in the config is
//!   nonetheless pointless or surprising. Report it; start anyway.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The config cannot be carried out as written. Blocks a run from
    /// starting — see `Graph::blocking`.
    Blocking,
    /// Worth saying; doesn't stop anything.
    Advisory,
}

/// One resolver finding. Serialized as an object rather than the bare
/// string it used to be, so the UI can render severity instead of calling
/// everything "warning" in the same amber.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub severity: Severity,
    pub message: String,
}

impl Warning {
    pub fn blocking(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Blocking,
            message: message.into(),
        }
    }

    pub fn advisory(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Advisory,
            message: message.into(),
        }
    }

    pub fn is_blocking(&self) -> bool {
        self.severity == Severity::Blocking
    }
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_serializes_snake_case() {
        let json = serde_json::to_string(&Warning::blocking("nope")).unwrap();
        assert!(json.contains(r#""severity":"blocking""#), "{json}");
        assert!(json.contains(r#""message":"nope""#), "{json}");
        let json = serde_json::to_string(&Warning::advisory("hm")).unwrap();
        assert!(json.contains(r#""severity":"advisory""#), "{json}");
    }
}
