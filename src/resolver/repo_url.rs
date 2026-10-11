pub fn repo_name_from_url(repo: &str) -> String {
    repo.rsplit('/')
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git")
        .to_string()
}

/// So `git@host:org/repo.git`, `https://host/org/repo.git` and
/// `ssh://host/org/repo` compare equal.
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
