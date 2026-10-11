//! What a resolver finding is worth: `Blocking` when the config can't be
//! carried out as written, `Advisory` otherwise.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Advisory,
}

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
