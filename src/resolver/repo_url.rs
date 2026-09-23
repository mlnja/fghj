//! Git remote URL parsing and normalization.

/// Derives the conventional local checkout folder name from a git URL: the
/// last path segment, with a trailing `.git` stripped.
pub fn repo_name_from_url(repo: &str) -> String {
    repo.rsplit('/')
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git")
        .to_string()
}

/// Normalizes a git URL to a form that's stable across `git@host:org/repo.git`,
/// `https://host/org/repo.git` and `ssh://host/org/repo` spellings of the same
/// repo, so two dependents referencing the same repo don't get treated as
/// different repos just because they wrote the URL (or its `local_path`
/// override) differently.
pub fn normalize_repo_url(repo: &str) -> String {
    let mut u = repo.trim().to_string();
    for prefix in ["ssh://", "https://", "http://"] {
        if let Some(rest) = u.strip_prefix(prefix) {
            u = rest.to_string();
            break;
        }
    }
    if let Some(rest) = u.strip_prefix("git@") {
        u = rest.replacen(':', "/", 1);
    }
    u.trim_end_matches(".git")
        .trim_end_matches('/')
        .to_lowercase()
}
