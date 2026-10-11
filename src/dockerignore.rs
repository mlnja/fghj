//! `.dockerignore`, applied to the context tarball fghj builds itself: the
//! Docker CLI filters client-side, so the daemon never does it for us. The
//! pattern language is `moby/patternmatcher`'s, not gitignore's.

use std::path::Path;

/// Empty when unreadable: shipping too much beats losing sources.
#[derive(Debug, Default)]
pub struct Dockerignore {
    patterns: Vec<Pattern>,
}

#[derive(Debug)]
struct Pattern {
    exclusion: bool,
    segments: Vec<String>,
}

impl Dockerignore {
    pub fn load(context_dir: &Path) -> Self {
        match std::fs::read_to_string(context_dir.join(".dockerignore")) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut patterns = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (exclusion, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            // Every pattern is relative to the root already.
            let body = body.trim_start_matches('/');
            let segments: Vec<String> = body
                .split('/')
                .filter(|s| !s.is_empty() && *s != ".")
                .map(String::from)
                .collect();
            if segments.is_empty() {
                continue;
            }
            patterns.push(Pattern {
                exclusion,
                segments,
            });
        }
        Self { patterns }
    }

    /// Without a `!` pattern, excluded directories are never walked.
    pub fn has_exclusions(&self) -> bool {
        self.patterns.iter().any(|p| p.exclusion)
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Last match wins. Ancestors count, so `.git` excludes everything beneath it.
    pub fn excludes(&self, path: &str) -> bool {
        let mut excluded = false;
        for pattern in &self.patterns {
            // Only patterns that could flip the current verdict matter.
            if pattern.exclusion != excluded {
                continue;
            }
            if pattern.matches(path) || pattern.matches_any_ancestor(path) {
                excluded = !pattern.exclusion;
            }
        }
        excluded
    }
}

impl Pattern {
    fn matches_any_ancestor(&self, path: &str) -> bool {
        let mut cut = 0;
        while let Some(i) = path[cut..].find('/') {
            cut += i;
            if self.matches(&path[..cut]) {
                return true;
            }
            cut += 1;
        }
        false
    }

    /// `*` and `?` stay within a segment; a `**` segment matches zero or more.
    /// Deviation: `**` glued to other characters (`a**b`) acts as `*`; nobody
    /// writes that.
    fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        matches_segments(&self.segments, &parts)
    }
}

fn matches_segments(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        // `foo` doesn't match `foo/bar` here; the ancestor check does that, which
        // keeps `!` able to re-include inside an excluded tree.
        None => path.is_empty(),
        Some((head, rest)) if head == "**" => {
            (0..=path.len()).any(|skip| matches_segments(rest, &path[skip..]))
        }
        Some((head, rest)) => match path.split_first() {
            Some((first, tail)) if matches_one(head, first) => matches_segments(rest, tail),
            _ => false,
        },
    }
}

fn matches_one(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // `star` lets a failed tail resume one character later.
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aikifactory() -> Dockerignore {
        Dockerignore::parse(".local\n.git\n.env\n*.test\n")
    }

    #[test]
    fn a_named_directory_excludes_everything_beneath_it() {
        let ignore = aikifactory();
        // Go stamps VCS info whenever `.git` is present, and fails without git.
        assert!(ignore.excludes(".git"));
        assert!(ignore.excludes(".git/HEAD"));
        assert!(ignore.excludes(".git/objects/ab/cdef"));
        assert!(ignore.excludes(".local/bigpkg-500mb.jar"));
        assert!(ignore.excludes(".env"));
    }

    #[test]
    fn the_sources_a_build_actually_needs_survive() {
        let ignore = aikifactory();
        for keep in [
            "go.mod",
            "go.sum",
            "Dockerfile",
            "cmd/aikifactory/main.go",
            "internal/server/server.go",
        ] {
            assert!(!ignore.excludes(keep), "{keep} should have been kept");
        }
    }

    #[test]
    fn a_star_stays_inside_one_path_segment() {
        let ignore = Dockerignore::parse("*.test\n");
        assert!(ignore.excludes("handler.test"));
        assert!(!ignore.excludes("pkg/handler.test"));

        let deep = Dockerignore::parse("**/*.test\n");
        assert!(deep.excludes("pkg/handler.test"));
        assert!(deep.excludes("a/b/c/handler.test"));
        assert!(deep.excludes("handler.test"));
    }

    #[test]
    fn a_later_exception_wins_over_an_earlier_exclusion() {
        let ignore = Dockerignore::parse("secrets\n!secrets/public.pem\n");
        assert!(ignore.excludes("secrets/private.pem"));
        assert!(!ignore.excludes("secrets/public.pem"));
        assert!(ignore.has_exclusions());
    }

    #[test]
    fn order_decides_and_not_specificity() {
        // Last match wins, not most specific.
        let ignore = Dockerignore::parse("!secrets/public.pem\nsecrets\n");
        assert!(ignore.excludes("secrets/public.pem"));
    }

    #[test]
    fn the_common_file_needs_no_directory_walk() {
        assert!(!aikifactory().has_exclusions());
        assert!(Dockerignore::parse("a\n!a/b\n").has_exclusions());
    }

    #[test]
    fn comments_blank_lines_and_leading_slashes_are_handled() {
        let ignore = Dockerignore::parse("# a comment\n\n  /build  \n./dist\n");
        assert!(ignore.excludes("build/out.o"));
        assert!(ignore.excludes("dist/app.js"));
        assert!(!ignore.excludes("src/main.go"));
        assert!(Dockerignore::parse("a#b\n").excludes("a#b"));
    }

    #[test]
    fn a_question_mark_matches_exactly_one_character() {
        let ignore = Dockerignore::parse("log.?\n");
        assert!(ignore.excludes("log.1"));
        assert!(!ignore.excludes("log.10"));
        assert!(!ignore.excludes("log."));
    }

    #[test]
    fn an_absent_or_empty_file_excludes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ignore = Dockerignore::load(dir.path());
        assert!(ignore.is_empty());
        assert!(!ignore.excludes("anything/at/all"));
        assert!(Dockerignore::parse("\n# nothing\n\n").is_empty());
    }

    #[test]
    fn a_star_backtracks_rather_than_committing_to_its_first_match() {
        assert!(matches_one("*b", "aab"));
        assert!(matches_one("a*b*c", "axxbyyc"));
        assert!(matches_one("*", "anything"));
        assert!(!matches_one("a*c", "abd"));
        assert!(matches_one("**", "treated-as-star-here"));
    }

    /// Why this isn't the `ignore` crate: a bare `node_modules` is anchored at the
    /// root here, but matches at any depth in gitignore. Verified against a real
    /// daemon.
    #[test]
    fn patterns_anchor_at_the_context_root_unlike_gitignore() {
        let ignore = Dockerignore::parse("node_modules\n");
        assert!(ignore.excludes("node_modules"));
        assert!(ignore.excludes("node_modules/r.txt"));
        assert!(!ignore.excludes("pkg/node_modules"));
        assert!(!ignore.excludes("pkg/node_modules/n.txt"));
        let deep = Dockerignore::parse("**/node_modules\n");
        assert!(deep.excludes("node_modules/r.txt"));
        assert!(deep.excludes("pkg/node_modules/n.txt"));
    }

    #[test]
    fn a_middle_double_star_spans_any_depth() {
        let ignore = Dockerignore::parse("node_modules/**/test\n");
        assert!(ignore.excludes("node_modules/test"));
        assert!(ignore.excludes("node_modules/a/test"));
        assert!(ignore.excludes("node_modules/a/b/test"));
        assert!(!ignore.excludes("node_modules/a/b/testing"));
    }
}
