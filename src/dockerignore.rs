//! `.dockerignore`, applied to the context tarball `fghj` builds itself.
//!
//! The Docker CLI filters a build context *client-side* before uploading it,
//! so `.dockerignore` is not something the daemon or BuildKit will do on
//! fghj's behalf. A client that tars the directory itself — which fghj does,
//! since the Engine API has no "build from this path" call — has to implement
//! the file's semantics or silently not honour it.
//!
//! Not honouring it is not a cosmetic difference. The bug that prompted this
//! module: a repo whose `.dockerignore` excluded `.git` built on the command
//! line and failed under fghj, because Go stamps VCS information into a binary
//! whenever it finds a `.git` directory beside the source, and `git` inside
//! the builder exits 128. The Dockerfile was fine; the context wasn't. The
//! same repo also shipped 572 MB of `.local` jars — and a private key and a
//! production JWT — into image layers on every build.
//!
//! The pattern language is Go's `filepath.Match` as extended by
//! `moby/patternmatcher`, not gitignore's, and the two disagree. Ported
//! faithfully enough for real `.dockerignore` files, with one documented
//! deviation (see [`Pattern::matches`]).

use std::path::Path;

/// A parsed `.dockerignore`.
///
/// An empty one (no file, or nothing but comments) excludes nothing, which is
/// the pre-existing behaviour and so the safe failure mode: a `.dockerignore`
/// fghj cannot read ships more than it should, never less.
#[derive(Debug, Default)]
pub struct Dockerignore {
    patterns: Vec<Pattern>,
}

#[derive(Debug)]
struct Pattern {
    /// `!`-prefixed: re-includes a path an earlier pattern excluded.
    exclusion: bool,
    segments: Vec<String>,
}

impl Dockerignore {
    /// Reads `<context_dir>/.dockerignore`, treating an unreadable or absent
    /// file as empty.
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
            // `#` is a comment only at the start of a line; a `#` inside a
            // pattern is a literal character in a filename.
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (exclusion, body) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim()),
                None => (false, line),
            };
            // Leading `/` is dropped rather than anchoring: every pattern is
            // already relative to the context root, so `/foo` and `foo` are
            // the same pattern.
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

    /// Whether any pattern can re-include something under an excluded
    /// directory.
    ///
    /// This is what decides whether an excluded directory can be skipped
    /// without being walked. Without a `!` pattern anywhere — the overwhelmingly
    /// common case, and the one that matters for speed — `.git` and a 572 MB
    /// `.local` are never opened at all.
    pub fn has_exclusions(&self) -> bool {
        self.patterns.iter().any(|p| p.exclusion)
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether `path` is excluded, counting a match on any ancestor directory.
    ///
    /// `path` is relative to the context root, `/`-separated, no leading `./`.
    ///
    /// Last match wins, so `**/*.jar` followed by `!keep/one.jar` keeps that
    /// one file. Ancestors are tested too, because `.git` must exclude
    /// everything beneath it without the file needing to list `.git/**`.
    pub fn excludes(&self, path: &str) -> bool {
        let mut excluded = false;
        for pattern in &self.patterns {
            // An exclusion pattern can only ever flip a path that is currently
            // excluded; an ordinary one only a path that isn't. Skipping the
            // rest is what makes last-match-wins cheap.
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

    /// Segment-wise glob match.
    ///
    /// `*` and `?` stay within one path segment, as in Go's `filepath.Match`;
    /// a segment of exactly `**` matches zero or more segments.
    ///
    /// **Deviation:** `moby` lets `**` cross separators even when glued to
    /// other characters in a segment (`a**b`), because it compiles patterns to
    /// a regex. Here such a `**` behaves as a single `*`. Real `.dockerignore`
    /// files write `**` as its own segment, and matching that shape exactly
    /// matters more than matching a form nobody writes.
    fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        matches_segments(&self.segments, &parts)
    }
}

fn matches_segments(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        // A pattern that runs out matches only a path that also has: `foo`
        // does not match `foo/bar` on its own — that is the ancestor check's
        // job, which keeps `!` able to re-include inside an excluded tree.
        None => path.is_empty(),
        Some((head, rest)) if head == "**" => {
            // Zero or more segments: try every split point.
            (0..=path.len()).any(|skip| matches_segments(rest, &path[skip..]))
        }
        Some((head, rest)) => match path.split_first() {
            Some((first, tail)) if matches_one(head, first) => matches_segments(rest, tail),
            _ => false,
        },
    }
}

/// Glob within a single path segment: `*` any run of characters, `?` exactly
/// one. Backtracking rather than regex, which keeps the module dependency-free.
fn matches_one(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // `star` remembers the last `*` so a failed tail can resume one character
    // later, which is what makes `*b` match `aab`.
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

    /// The exact file from the repo whose build this module was written to fix.
    fn aikifactory() -> Dockerignore {
        Dockerignore::parse(".local\n.git\n.env\n*.test\n")
    }

    #[test]
    fn a_named_directory_excludes_everything_beneath_it() {
        let ignore = aikifactory();
        // The whole point: `.git` as a bare name has to take the tree with it,
        // or Go still finds a repository to stamp from.
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
        // `*` must not eat the separator, or `*.test` would also match
        // `pkg/handler.test` — which Docker's own matcher does not.
        assert!(!ignore.excludes("pkg/handler.test"));

        let deep = Dockerignore::parse("**/*.test\n");
        assert!(deep.excludes("pkg/handler.test"));
        assert!(deep.excludes("a/b/c/handler.test"));
        // `**` matches zero segments too.
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
        // Reversed, the exception is overruled: Docker is last-match-wins, not
        // most-specific-wins, and a file relying on the other reading would
        // silently ship a key.
        let ignore = Dockerignore::parse("!secrets/public.pem\nsecrets\n");
        assert!(ignore.excludes("secrets/public.pem"));
    }

    #[test]
    fn the_common_file_needs_no_directory_walk() {
        // What lets `.local`'s 572 MB be skipped without being opened.
        assert!(!aikifactory().has_exclusions());
        assert!(Dockerignore::parse("a\n!a/b\n").has_exclusions());
    }

    #[test]
    fn comments_blank_lines_and_leading_slashes_are_handled() {
        let ignore = Dockerignore::parse("# a comment\n\n  /build  \n./dist\n");
        assert!(ignore.excludes("build/out.o"));
        assert!(ignore.excludes("dist/app.js"));
        assert!(!ignore.excludes("src/main.go"));
        // A `#` mid-pattern is a filename character, not a comment.
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
        // The safe direction: a context fghj cannot filter ships too much, a
        // build that silently lost its sources would be far harder to diagnose.
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

    /// The semantic that rules out the `ignore` crate, and the reason this
    /// module is hand-rolled rather than borrowed.
    ///
    /// `.dockerignore` patterns are anchored at the context root: a bare
    /// `node_modules` excludes the top-level one and *not* `pkg/node_modules`.
    /// gitignore matches a bare name at any depth, so `ignore` — the obvious
    /// crate to reach for, and a far better-tested one than this — would filter
    /// contexts differently from `docker build`. Verified against a real daemon
    /// before relying on it: a build whose `.dockerignore` held only
    /// `node_modules` received `pkg/node_modules/n.txt` and not
    /// `node_modules/r.txt`.
    #[test]
    fn patterns_anchor_at_the_context_root_unlike_gitignore() {
        let ignore = Dockerignore::parse("node_modules\n");
        assert!(ignore.excludes("node_modules"));
        assert!(ignore.excludes("node_modules/r.txt"));
        assert!(!ignore.excludes("pkg/node_modules"));
        assert!(!ignore.excludes("pkg/node_modules/n.txt"));
        // Reaching nested copies is opt-in, and this is how a file asks.
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
