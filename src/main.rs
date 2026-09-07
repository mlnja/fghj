use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

const SCHEMA_DEPENDENCY: &str = include_str!("../schema/dependency.cue");
const SCHEMA_COMPONENT: &str = include_str!("../schema/component.cue");

#[derive(Parser)]
#[command(
    name = "fghj",
    version,
    about = "Local development orchestration for user flows"
)]
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
    /// Reconcile fghjd back into the active state (occupy 80/443 and DNS,
    /// resync /etc/hosts)
    Start,
    /// Release 80/443, DNS, and /etc/hosts without stopping fghjd itself
    Stop,
    /// Equivalent to `stop` followed by `start`
    Restart,
    /// Report whether fghjd is running and whether it's active or idle
    Status,
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
    let workspace = fghj::resolve_workspace(Some(entry), workspace, None)?;
    let g = fghj::resolver::resolve_universe(&workspace)?;
    println!("{}", serde_json::to_string_pretty(&g)?);
    Ok(())
}

fn probe_daemon() -> bool {
    UnixStream::connect(fghj::daemon::socket_path()).is_ok()
}

/// Sends a bare-bones HTTP request (no body for `GET`) over `fghjd`'s Unix
/// control socket and parses the JSON response body. The API has tiny
/// bodies and no need for keep-alive, so a hand-rolled request avoids
/// pulling in an HTTP client crate for these few call sites.
fn http_request_json(
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    let socket_path = fghj::daemon::socket_path();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut stream = UnixStream::connect(&socket_path).with_context(|| {
        format!(
            "failed to connect to fghjd's control socket at {} — is fghjd running? (`sudo fghjd`)",
            socket_path.display()
        )
    })?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: fghjd\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(request.as_bytes())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let json_str = &text[body_start..];
    serde_json::from_str(json_str)
        .with_context(|| format!("invalid response from fghjd: {json_str}"))
}

fn http_post_json(path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    http_request_json("POST", path, Some(body))
}

fn http_get_json(path: &str) -> Result<serde_json::Value> {
    http_request_json("GET", path, None)
}

fn wire(entry: String, workspace: Option<PathBuf>) -> Result<()> {
    if !probe_daemon() {
        bail!(
            "fghjd isn't running — start it with `sudo fghjd` in another terminal \
             (or install it as a system service; see SPEC.md)"
        );
    }

    let workspace = workspace.unwrap_or_else(|| PathBuf::from("."));
    let absolute_workspace = if workspace.is_absolute() {
        workspace
    } else {
        std::env::current_dir()?.join(workspace)
    };

    // `fghjd` runs as root and has no credentials of its own for private
    // remotes — captured here (as the real, unprivileged user) so the daemon
    // can later drop privileges back to this user before shelling out to
    // `git clone`. Only sent when HOME is known; the daemon falls back to
    // its old (root-only) behavior otherwise.
    let owner = std::env::var("HOME").ok().map(|home| {
        serde_json::json!({
            "uid": unsafe { libc::getuid() },
            "gid": unsafe { libc::getgid() },
            "home": home,
            "ssh_auth_sock": std::env::var("SSH_AUTH_SOCK").ok(),
        })
    });

    let resp = http_post_json(
        "/workspaces",
        &serde_json::json!({ "entry": entry, "workspace": absolute_workspace, "owner": owner }),
    )?;
    if let Some(err) = resp.get("error") {
        bail!("fghjd rejected workspace: {err}");
    }
    let id = resp["id"]
        .as_str()
        .context("fghjd response missing workspace id")?;

    println!("data is wired — open the UI to see it: https://fghj.internal/?workspace={id}");

    Ok(())
}

/// `fghjd` itself is meant to run for the life of the machine, supervised by
/// launchd/systemd — these commands never touch that process's lifecycle.
/// They talk to its always-on control API to toggle whether it's actively
/// occupying 80/443, `*.fghj.internal` DNS, and `/etc/hosts`, or sitting
/// idle out of the way (see `fghj::daemon::DaemonControl`).
fn daemon_start() -> Result<()> {
    if !probe_daemon() {
        bail!(
            "fghjd isn't running — start the service first (e.g. `sudo brew services start fghj`, \
             or `sudo fghjd` in the foreground for local dev)"
        );
    }
    let resp = http_post_json("/daemon/start", &serde_json::json!({}))?;
    if let Some(err) = resp.get("error") {
        bail!("fghjd failed to activate: {err}");
    }
    println!("fghjd is active — occupying 80/443 and *.fghj.internal DNS");
    Ok(())
}

fn daemon_stop() -> Result<()> {
    if !probe_daemon() {
        println!("fghjd is not running");
        return Ok(());
    }
    http_post_json("/daemon/stop", &serde_json::json!({}))?;
    println!(
        "fghjd is now idle — 80/443, DNS, and /etc/hosts released (fghjd itself is still running; \
         `fghj daemon start` to reconcile again)"
    );
    Ok(())
}

fn daemon_restart() -> Result<()> {
    daemon_stop()?;
    daemon_start()
}

fn daemon_status() -> Result<()> {
    if !probe_daemon() {
        println!("fghjd is not running");
        return Ok(());
    }
    let resp = http_get_json("/daemon/status")?;
    let active = resp["active"].as_bool().unwrap_or(false);
    println!(
        "fghjd is running and {}",
        if active { "active" } else { "idle" }
    );
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Validate { path } => validate(&path),
        Commands::Graph { entry, workspace } => graph(entry, workspace),
        Commands::Wire { entry, workspace } => wire(entry, workspace),
        Commands::Daemon { action } => match action {
            DaemonAction::Start => daemon_start(),
            DaemonAction::Stop => daemon_stop(),
            DaemonAction::Restart => daemon_restart(),
            DaemonAction::Status => daemon_status(),
        },
    }
}
