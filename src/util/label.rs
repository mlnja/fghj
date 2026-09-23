/// Folds arbitrary text down to a `[a-z0-9-]` slug: lowercased, every run of
/// non-alphanumeric characters collapsed to a single `-`, and no leading or
/// trailing `-`. The shape Docker accepts for a container, network or volume
/// name, and the shape a DNS label accepts — so the same function serves
/// both.
pub fn sanitize_label(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::new();
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_label_lowercases_and_collapses_separators() {
        assert_eq!(
            sanitize_label("Feature/JIRA-123 Fix"),
            "feature-jira-123-fix"
        );
        assert_eq!(sanitize_label("already-clean"), "already-clean");
        assert_eq!(sanitize_label("__leading__"), "leading");
    }
}
