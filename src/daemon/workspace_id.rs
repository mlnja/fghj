//! Deriving the stable per-workspace id from its canonical path.

use std::path::Path;

/// Deterministic, URL-safe id for a canonicalized workspace path (FNV-1a of
/// the path, prefixed with a readable slug of its directory name), so
/// repeated `fghj ui` calls against the same directory land on the same
/// workspace instead of registering a duplicate.
pub fn workspace_id(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let slug: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("{slug}-{hash:012x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_is_deterministic_and_path_specific() {
        let a = workspace_id(Path::new("/tmp/fixtures/foo"));
        let b = workspace_id(Path::new("/tmp/fixtures/foo"));
        let c = workspace_id(Path::new("/tmp/fixtures/bar"));
        assert_eq!(a, b, "same path must hash to the same id");
        assert_ne!(a, c, "different paths must not collide");
        assert!(
            a.starts_with("foo-"),
            "id should carry a readable slug: {a}"
        );
    }
}
