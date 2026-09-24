//! Finding and reading every `.fghj.yaml` on disk in the workspace.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::config::ComponentConfig;
use super::warning::Warning;
use anyhow::{Context, Result};

pub fn read_component_file(path: &Path) -> Result<ComponentConfig> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse {} as a component config", path.display()))
}

/// What a scan found, and what it could not read.
///
/// The split exists because a workspace is federated: the repos in it are
/// independent, and one of them having a broken file is not a statement
/// about any of the others. Returning a bare `Result<BTreeMap<..>>` made
/// every repo's fate depend on every other repo's worst file.
pub struct ScannedWorkspace {
    /// Keyed by local folder name — the convention-based repo id.
    pub components: BTreeMap<String, ComponentConfig>,
    /// One per repo whose `.fghj.yaml` exists but could not be read or
    /// parsed. Always blocking: see [`scan_workspace`].
    pub warnings: Vec<Warning>,
}

/// Reads every immediate subdirectory of `workspace` that contains an
/// `.fghj.yaml`, keyed by its local folder name (the convention-based repo
/// id).
///
/// A repo whose `.fghj.yaml` is unreadable or malformed is **skipped and
/// reported**, not propagated. Aborting the whole scan contradicted
/// [[flat-workspace-model]]'s lazy/partial resolution: one bad file
/// anywhere 500'd the graph endpoint, so the UI could not render the very
/// workspace you needed to look at in order to find the bad file.
///
/// It is skipped rather than stubbed, though, and the difference matters. A
/// *missing* repo legitimately stubs — fghj genuinely does not know what it
/// declares yet, and saying so is honest. A *malformed* repo is present and
/// says something fghj cannot read; treating it as "declares nothing" would
/// be substituting a guess for the author's intent, quietly. So it becomes
/// a `Severity::Blocking` warning: you can see the workspace, and starts
/// refuse until the file is fixed.
///
/// The `Result` is reserved for the workspace directory itself being
/// unreadable, which is not a per-repo problem and leaves nothing to
/// partially resolve.
pub fn scan_workspace(workspace: &Path) -> Result<ScannedWorkspace> {
    let mut components = BTreeMap::new();
    let mut warnings = Vec::new();
    if !workspace.exists() {
        return Ok(ScannedWorkspace {
            components,
            warnings,
        });
    }
    for entry in fs::read_dir(workspace)
        .with_context(|| format!("failed to read workspace dir {}", workspace.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let config_path = path.join(".fghj.yaml");
        if !config_path.exists() {
            continue;
        }
        let local_path = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        match read_component_file(&config_path) {
            Ok(component) => {
                // Accepted — a newer minor is the whole point of the
                // major/minor split — but never silently. The features
                // added after this build's minor are simply not here, and
                // config that reads as if it were honoured is exactly the
                // kind of quiet wrongness worth a line on screen.
                if component.version.is_newer_minor_than_supported() {
                    warnings.push(Warning::advisory(format!(
                        "'{local_path}' declares version {} but this fghj implements {};                          anything added after {} is ignored",
                        component.version,
                        super::version::SCHEMA_VERSION,
                        super::version::SCHEMA_VERSION
                    )));
                }
                components.insert(local_path, component);
            }
            // `{e:#}` so the serde_yaml line/column survives into the
            // message — "did not find expected key at line 7 column 3" is
            // the whole value of this warning.
            Err(e) => warnings.push(Warning::blocking(format!(
                "'{local_path}' has an unreadable .fghj.yaml, so nothing it declares is \
                 in the graph: {e:#}"
            ))),
        }
    }
    Ok(ScannedWorkspace {
        components,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with(repos: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, yaml) in repos {
            let repo = dir.path().join(name);
            fs::create_dir_all(&repo).expect("mkdir");
            fs::write(repo.join(".fghj.yaml"), yaml).expect("write");
        }
        dir
    }

    const GOOD: &str =
        "version: \"1.0\"\nservices:\n  web:\n    build:\n      dockerfile: Dockerfile\n";

    #[test]
    fn a_clean_workspace_reports_no_warnings() {
        let dir = workspace_with(&[("alpha", GOOD), ("beta", GOOD)]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert_eq!(scanned.components.len(), 2);
        assert!(scanned.warnings.is_empty(), "{:?}", scanned.warnings);
    }

    /// The whole point of B11: the good repo still resolves.
    #[test]
    fn a_malformed_repo_is_reported_without_taking_the_others_with_it() {
        let dir = workspace_with(&[("good", GOOD), ("broken", "services: [this is not a map")]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.contains_key("good"));
        assert!(!scanned.components.contains_key("broken"));
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(scanned.warnings[0].message.contains("broken"));
    }

    /// Blocking, not advisory — a present-but-unreadable repo is a hole in
    /// the graph, and starting around it would bring up a subset nobody
    /// asked for. Visible, yes; startable, no.
    #[test]
    fn a_malformed_repo_blocks_the_start() {
        let dir = workspace_with(&[("broken", "\t- tabs are not valid yaml indentation")]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(scanned.warnings[0].is_blocking());
    }

    /// Each bad repo earns its own message; one does not mask the next.
    #[test]
    fn every_malformed_repo_is_named_separately() {
        let dir = workspace_with(&[
            ("bad-one", "services: ["),
            ("bad-two", "services: ["),
            ("good", GOOD),
        ]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert_eq!(scanned.components.len(), 1);
        assert_eq!(scanned.warnings.len(), 2, "{:?}", scanned.warnings);
        let joined = scanned
            .warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined.contains("bad-one") && joined.contains("bad-two"),
            "{joined}"
        );
    }

    /// Staggered adoption, the thing [E8] said was impossible: the repo
    /// resolves normally, and the gap is stated rather than hidden.
    #[test]
    fn a_newer_minor_version_is_used_and_flagged_as_advisory() {
        let dir = workspace_with(&[(
            "ahead",
            "version: \"1.9\"\nservices:\n  web:\n    build:\n      dockerfile: Dockerfile\n",
        )]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.contains_key("ahead"));
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(!scanned.warnings[0].is_blocking());
        assert!(
            scanned.warnings[0].message.contains("1.9"),
            "{:?}",
            scanned.warnings
        );
    }

    /// A different *major* is a barrier, not a note — and it arrives
    /// through the same per-repo isolation as any other unreadable file.
    #[test]
    fn a_different_major_version_makes_the_repo_unreadable() {
        let dir = workspace_with(&[
            ("future", "version: \"2.0\"\nservices: {}\n"),
            ("good", GOOD),
        ]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.contains_key("good"));
        assert!(!scanned.components.contains_key("future"));
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(scanned.warnings[0].is_blocking());
        assert!(
            scanned.warnings[0].message.contains("1.x"),
            "{:?}",
            scanned.warnings
        );
    }

    /// An invalid service name used to parse fine and reach DNS unescaped
    /// (B8). Now it makes its repo unreadable, reported with the file and
    /// line, while its peers resolve.
    #[test]
    fn an_invalid_service_name_is_rejected_at_parse_time() {
        let dir = workspace_with(&[
            (
                "bad-name",
                "version: \"1.0\"\nservices:\n  \"My Service!\":\n    build: {}\n",
            ),
            ("good", GOOD),
        ]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.contains_key("good"));
        assert!(!scanned.components.contains_key("bad-name"));
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(scanned.warnings[0].is_blocking());
        assert!(
            scanned.warnings[0].message.contains("uppercase"),
            "{:?}",
            scanned.warnings
        );
    }

    /// A directory with no `.fghj.yaml` at all is not a repo fghj has an
    /// opinion about — it is not a failure and must not be reported as one.
    #[test]
    fn a_directory_without_a_component_file_is_silently_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("not-a-repo")).expect("mkdir");
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.is_empty());
        assert!(scanned.warnings.is_empty(), "{:?}", scanned.warnings);
    }

    /// The workspace folder not existing yet is the normal pre-`pull` state.
    #[test]
    fn a_missing_workspace_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scanned = scan_workspace(&dir.path().join("nope")).expect("scan");
        assert!(scanned.components.is_empty());
        assert!(scanned.warnings.is_empty());
    }
}
