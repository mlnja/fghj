use std::fs;
use std::path::Path;

/// `service_yaml` is the body under `services:\n  {local_path}:`. One naming
/// neither `build` nor `image` gets `build: .`.
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

pub fn write_yaml(workspace: &Path, local_path: &str, yaml: &str) {
    let dir = workspace.join(local_path);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".fghj.yaml"), yaml).unwrap();
}

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
mod flows;
mod hosts;
mod ports;
mod run_options;
mod scenarios;
mod stores;
mod tasks;
mod volumes;
