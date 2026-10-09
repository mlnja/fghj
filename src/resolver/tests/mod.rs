//! Shared fixtures and the per-feature test modules for the resolver.

use std::fs;
use std::path::Path;

/// Writes a component config with a single service named after
/// `local_path` (matching every test's own convention) — `service_yaml`
/// is the body that used to sit directly under a singular `service:`
/// field (no `name:` line; the map key is the name now), reindented one
/// level deeper to sit under `services:\n  {local_path}:`.
///
/// A body that names neither `build` nor `image` gets `build: .`, so the
/// node is the repo's own service — which is what nearly every test means.
pub fn write_component(workspace: &Path, local_path: &str, service_yaml: &str) {
    let dir = workspace.join(local_path);
    fs::create_dir_all(&dir).unwrap();
    let has_source = service_yaml
        .lines()
        .any(|l| matches!(l.trim_start().split(':').next(), Some("build" | "image")));
    let service_yaml = if has_source {
        service_yaml.to_string()
    } else {
        format!("  build: .\n{service_yaml}")
    };
    let indented: String = service_yaml
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!("  {local_path}:\n{indented}\n");
    fs::write(
        dir.join(".fghj.yaml"),
        format!("version: \"2.0\"\nservices:\n{body}"),
    )
    .unwrap();
}

/// Writes `yaml` verbatim as `local_path`'s `.fghj.yaml`, for a test that
/// needs more than one service, an `include:` or `flows:`.
pub fn write_yaml(workspace: &Path, local_path: &str, yaml: &str) {
    let dir = workspace.join(local_path);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".fghj.yaml"), yaml).unwrap();
}

/// Makes `workspace/local_path` a git checkout whose `origin` is `url`, so
/// an `include:` of `url` finds it.
pub fn git_init(workspace: &Path, local_path: &str, url: &str) {
    let dir = workspace.join(local_path);
    fs::create_dir_all(&dir).unwrap();
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", url]] {
        let ok = std::process::Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }
}

mod build;
mod depends_on;
mod domains;
mod flows;
mod hosts;
mod ports;
mod run_options;
mod scenarios;
mod tasks;
mod volumes;
