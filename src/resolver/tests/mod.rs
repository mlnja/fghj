//! Shared fixtures and the per-feature test modules for the resolver.

use std::fs;
use std::path::Path;

/// Writes a component config with a single service named after
/// `local_path` (matching every test's own convention) — `service_yaml`
/// is the body that used to sit directly under a singular `service:`
/// field (no `name:` line; the map key is the name now), reindented one
/// level deeper to sit under `services:\n  {local_path}:`.
pub fn write_component(workspace: &Path, local_path: &str, service_yaml: &str) {
    let dir = workspace.join(local_path);
    fs::create_dir_all(&dir).unwrap();
    let indented: String = service_yaml
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = if indented.trim().is_empty() {
        format!("  {local_path}: {{}}\n")
    } else {
        format!("  {local_path}:\n{indented}\n")
    };
    fs::write(
        dir.join(".fghj.yaml"),
        format!("version: \"1.0\"\nservices:\n{body}"),
    )
    .unwrap();
}

mod dependencies;
mod domains;
mod hosts;
mod ports;
mod run_options;
mod volumes;
