/// Valid as a Docker object name and as a DNS label.
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
