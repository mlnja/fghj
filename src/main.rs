use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

const SCHEMA_DEPENDENCY: &str = include_str!("../schema/dependency.cue");
const SCHEMA_COMPONENT: &str = include_str!("../schema/component.cue");

#[derive(Parser)]
#[command(name = "fghj", about = "Local development orchestration for user flows")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate an fghj.yaml file against the CUE schema
    Validate {
        /// Path to the fghj.yaml file to validate
        path: PathBuf,
    },
    /// Resolve the full dependency universe (all flows) and print it as JSON
    Graph {
        /// Git URL of the entry repo to clone in, if not already in the workspace
        entry: String,
        /// Workspace root directory holding sibling repo checkouts (default: current directory)
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Wire a workspace into the running fghjd daemon so it shows up in the UI
    Wire {
        /// Git URL of the entry repo to clone in, if not already in the workspace
        entry: String,
        /// Workspace root directory holding sibling repo checkouts (default: current directory)
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Manage the fghjd background daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Stop fghjd and every workspace it was serving
    Stop,
}

fn validate(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }

    let schema_dir = tempfile::tempdir().context("failed to create temp dir for schema files")?;
    fs::write(schema_dir.path().join("dependency.cue"), SCHEMA_DEPENDENCY)?;
    fs::write(schema_dir.path().join("component.cue"), SCHEMA_COMPONENT)?;

    let output = Command::new("cue")
        .arg("vet")
        .arg(path)
        .arg(schema_dir.path().join("dependency.cue"))
        .arg(schema_dir.path().join("component.cue"))
        .arg("-d")
        .arg("#ComponentConfig")
        .output()
        .context("failed to run `cue` — is it installed and on your PATH? (https://cuelang.org/docs/install/)")?;

    if output.status.success() {
        println!("{} is a valid component config", path.display());
        Ok(())
    } else {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        bail!("{} failed schema validation", path.display());
    }
}

fn graph(entry: String, workspace: Option<PathBuf>) -> Result<()> {
    let workspace = fghj::resolve_workspace(Some(entry), workspace)?;
    let g = fghj::resolver::resolve_universe(&workspace)?;
    println!("{}", serde_json::to_string_pretty(&g)?);
    Ok(())
}

fn probe_daemon() -> bool {
    TcpStream::connect(("127.0.0.1", fghj::daemon::CONTROL_PORT)).is_ok()
}

/// Sends a JSON POST over a raw TCP connection and parses the JSON response
/// body. `fghjd`'s control API is localhost-only with tiny bodies, so a
/// hand-rolled request avoids pulling in an HTTP client crate for this one
/// call site.
fn http_post_json(path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let port = fghj::daemon::CONTROL_PORT;
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .with_context(|| format!("failed to connect to fghjd control API on port {port}"))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let json_str = &text[body_start..];
    serde_json::from_str(json_str).with_context(|| format!("invalid response from fghjd: {json_str}"))
}

fn wire(entry: String, workspace: Option<PathBuf>) -> Result<()> {
    if !probe_daemon() {
        bail!(
            "fghjd isn't running on 127.0.0.1:{} — start it with `sudo fghjd` in another terminal \
             (or install it as a system service; see SPEC.md)",
            fghj::daemon::CONTROL_PORT
        );
    }

    let workspace = workspace.unwrap_or_else(|| PathBuf::from("."));
    let absolute_workspace = if workspace.is_absolute() {
        workspace
    } else {
        std::env::current_dir()?.join(workspace)
    };

    let resp = http_post_json(
        "/workspaces",
        &serde_json::json!({ "entry": entry, "workspace": absolute_workspace }),
    )?;
    if let Some(err) = resp.get("error") {
        bail!("fghjd rejected workspace: {err}");
    }
    let id = resp["id"].as_str().context("fghjd response missing workspace id")?;

    let url = format!("http://127.0.0.1:{}/?workspace={id}", fghj::daemon::CONTROL_PORT);
    println!("data is wired — open the UI to see it: {url}");

    Ok(())
}

fn daemon_stop() -> Result<()> {
    match fghj::daemon::read_pid() {
        Some(pid) if fghj::daemon::pid_alive(pid) => {
            println!("stopping fghjd (pid {pid}, requires sudo)...");
            let pidfile = fghj::daemon::pid_path();
            let cmd = format!("kill {pid} && rm -f {}", pidfile.display());
            let status = Command::new("sudo")
                .args(["sh", "-c", &cmd])
                .status()
                .context("failed to run sudo to stop fghjd")?;
            if !status.success() {
                bail!("failed to stop fghjd (pid {pid})");
            }
            println!("fghjd stopped");
        }
        _ => println!("fghjd is not running"),
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Validate { path } => validate(&path),
        Commands::Graph { entry, workspace } => graph(entry, workspace),
        Commands::Wire { entry, workspace } => wire(entry, workspace),
        Commands::Daemon { action: DaemonAction::Stop } => daemon_stop(),
    }
}
