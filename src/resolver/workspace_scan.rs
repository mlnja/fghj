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

/// Split so one repo's broken file doesn't take down the others.
pub struct ScannedWorkspace {
    pub components: BTreeMap<String, ComponentConfig>,
    pub warnings: Vec<Warning>,
}

/// A repo whose `.fghj.yaml` is unreadable is skipped and reported as
/// blocking, not stubbed: fghj can't guess what it declares, but the rest of
/// the workspace still renders. `Err` only when the workspace itself can't be
/// read.
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
                if component.version.is_newer_minor_than_supported() {
                    warnings.push(Warning::advisory(format!(
                        "'{local_path}' declares version {} but this fghj implements {}; \
                         anything added after {} is ignored",
                        component.version,
                        super::version::SCHEMA_VERSION,
                        super::version::SCHEMA_VERSION
                    )));
                }
                components.insert(local_path, component);
            }
            // `{e:#}` keeps serde_yaml's line and column.
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
        "version: \"2.0\"\nservices:\n  web:\n    build:\n      dockerfile: Dockerfile\n";

    #[test]
    fn a_clean_workspace_reports_no_warnings() {
        let dir = workspace_with(&[("alpha", GOOD), ("beta", GOOD)]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert_eq!(scanned.components.len(), 2);
        assert!(scanned.warnings.is_empty(), "{:?}", scanned.warnings);
    }

    #[test]
    fn a_build_source_with_its_own_fghj_yaml_is_not_a_repo() {
        let source = format!("{}/geocoder@v1", super::super::build_source::SOURCES_DIR);
        let dir = workspace_with(&[("alpha", GOOD), (source.as_str(), GOOD)]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert_eq!(scanned.components.keys().collect::<Vec<_>>(), ["alpha"]);
    }

    #[test]
    fn a_malformed_repo_is_reported_without_taking_the_others_with_it() {
        let dir = workspace_with(&[("good", GOOD), ("broken", "services: [this is not a map")]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.contains_key("good"));
        assert!(!scanned.components.contains_key("broken"));
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(scanned.warnings[0].message.contains("broken"));
    }

    #[test]
    fn a_malformed_repo_blocks_the_start() {
        let dir = workspace_with(&[("broken", "\t- tabs are not valid yaml indentation")]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(scanned.warnings[0].is_blocking());
    }

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

    #[test]
    fn a_newer_minor_version_is_used_and_flagged_as_advisory() {
        let dir = workspace_with(&[(
            "ahead",
            "version: \"2.9\"\nservices:\n  web:\n    build:\n      dockerfile: Dockerfile\n",
        )]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.contains_key("ahead"));
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(!scanned.warnings[0].is_blocking());
        assert!(
            scanned.warnings[0].message.contains("2.9"),
            "{:?}",
            scanned.warnings
        );
    }

    #[test]
    fn a_different_major_version_makes_the_repo_unreadable() {
        let dir = workspace_with(&[
            ("future", "version: \"3.0\"\nservices: {}\n"),
            ("good", GOOD),
        ]);
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.contains_key("good"));
        assert!(!scanned.components.contains_key("future"));
        assert_eq!(scanned.warnings.len(), 1, "{:?}", scanned.warnings);
        assert!(scanned.warnings[0].is_blocking());
        assert!(
            scanned.warnings[0].message.contains("2.x"),
            "{:?}",
            scanned.warnings
        );
    }

    #[test]
    fn an_invalid_service_name_is_rejected_at_parse_time() {
        let dir = workspace_with(&[
            (
                "bad-name",
                "version: \"2.0\"\nservices:\n  \"My Service!\":\n    build: {}\n",
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

    #[test]
    fn a_directory_without_a_component_file_is_silently_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("not-a-repo")).expect("mkdir");
        let scanned = scan_workspace(dir.path()).expect("scan");
        assert!(scanned.components.is_empty());
        assert!(scanned.warnings.is_empty(), "{:?}", scanned.warnings);
    }

    #[test]
    fn a_missing_workspace_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scanned = scan_workspace(&dir.path().join("nope")).expect("scan");
        assert!(scanned.components.is_empty());
        assert!(scanned.warnings.is_empty());
    }
}
