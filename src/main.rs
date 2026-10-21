use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fghj::doctor::Verdict;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

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
    /// Validate an .fghj.yaml file against the CUE schema
    Validate {
        /// Path to the .fghj.yaml file to validate
        path: PathBuf,
    },
    /// Resolve the full dependency universe (all flows) and print it as JSON
    Graph {
        /// Git URL of the entry repo to clone in, if not already in the workspace;
        /// or a repo on disk — its `.fghj.yaml` or its folder — whose parent is
        /// the workspace
        entry: String,
        /// Workspace root holding sibling repo checkouts (default: the wired
        /// workspace the current directory is inside, else the current directory)
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Wire a workspace into the running fghjd daemon so it shows up in the UI
    Wire {
        /// Git URL of the entry repo to clone in, if not already in the workspace;
        /// or a repo on disk — its `.fghj.yaml` or its folder — whose parent is
        /// the workspace
        entry: String,
        /// Workspace root holding sibling repo checkouts (default: the wired
        /// workspace the current directory is inside, else the current directory)
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Manage the fghjd background daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Check that everything fghj needs from this machine is actually
    /// there: Docker with BuildKit, the proxy's ports and loopback alias,
    /// OS DNS routing, and the trusted root CA. Exits non-zero if anything
    /// is broken
    Doctor,
    /// Remove everything fghj installed on this machine: the trusted root
    /// CA, /var/lib/fghjd, and all system configuration. Needs root
    Uninstall {
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Leave the root CA trusted in the System keychain
        #[arg(long)]
        keep_ca: bool,
        /// Leave /var/lib/fghjd (wired workspaces, CA key, run state) in place
        #[arg(long)]
        keep_state: bool,
    },
    /// Run a command inside a running node's container — like `docker
    /// compose exec`, full duplex (a real interactive shell works)
    Exec {
        /// Node id to exec into (see `fghj graph` for ids)
        node: String,
        /// Workspace root holding sibling repo checkouts (default: the wired
        /// workspace the current directory is inside, else the current directory)
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Disable pseudo-TTY allocation even if stdin/stdout are TTYs
        #[arg(short = 'T', long)]
        no_tty: bool,
        /// Command and arguments to run inside the container
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Reconcile fghjd back into the active state (serve 80/443 and DNS,
    /// resync /etc/hosts)
    Start,
    /// Release 80/443, the loopback alias, DNS, and /etc/hosts without stopping fghjd itself
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

/// An explicit flag wins; otherwise the wired workspace containing the current
/// directory. Canonical, so `/var` and `/private/var` match. Falls back to the
/// absolute path: `graph` may name a directory that doesn't exist yet.
fn workspace_root(flag: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if let Some(flag) = flag {
        let absolute = if flag.is_absolute() {
            flag
        } else {
            cwd.join(flag)
        };
        return Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute));
    }

    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    match derive_workspace(&cwd) {
        // On stderr, so `fghj graph`'s JSON stays clean. Silent when it's the
        // directory you're in.
        Some(derived) if derived != cwd => {
            eprintln!(
                "using workspace {} (derived from the current directory)",
                derived.display()
            );
            Ok(derived)
        }
        Some(derived) => Ok(derived),
        None => Ok(cwd),
    }
}

/// Via the API, since `workspaces.json` is root-only. Every failure is `None`:
/// `graph` works without a daemon.
fn derive_workspace(cwd: &Path) -> Option<PathBuf> {
    if !probe_daemon() {
        return None;
    }
    let workspaces = http_get_json("/workspaces").ok()?;
    let candidates: Vec<PathBuf> = workspaces
        .as_array()?
        .iter()
        .filter_map(|w| w["workspace"].as_str())
        .map(PathBuf::from)
        .collect();
    fghj::enclosing_workspace(cwd, &candidates)
}

/// `None` for the entry when it's a repo on disk: there is nothing to clone,
/// and its parent is the workspace.
fn entry_and_workspace(
    entry: &str,
    workspace: Option<PathBuf>,
) -> Result<(Option<String>, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(local) = local_workspace(entry, &cwd, home.as_deref())? else {
        return Ok((Some(entry.to_string()), workspace_root(workspace)?));
    };
    if let Some(flag) = workspace {
        let flag = workspace_root(Some(flag))?;
        if flag != local {
            bail!(
                "{entry} is a repo in {}, but --workspace names {}; drop --workspace, \
                 or wire a repo inside it",
                local.display(),
                flag.display()
            );
        }
    }
    Ok((None, local))
}

/// `None` when `entry` isn't on disk, so it's a git URL. A workspace's repos
/// are the folders right under it with a `.fghj.yaml`, so that's what this
/// accepts. Home and `/` are refused: every folder in them would be a repo.
fn local_workspace(entry: &str, cwd: &Path, home: Option<&Path>) -> Result<Option<PathBuf>> {
    let path = cwd.join(entry);
    if !path.exists() {
        return Ok(None);
    }
    let path = std::fs::canonicalize(&path)?;
    let repo = if path.is_dir() {
        if !path.join(".fghj.yaml").is_file() {
            bail!("{} has no .fghj.yaml", path.display());
        }
        path
    } else if path.file_name().is_some_and(|n| n == ".fghj.yaml") {
        path.parent().map(Path::to_path_buf).unwrap_or(path)
    } else {
        bail!(
            "{} isn't a .fghj.yaml; name the repo's .fghj.yaml or its folder",
            path.display()
        );
    };
    let workspace = match repo.parent() {
        Some(parent) if parent.parent().is_some() => parent.to_path_buf(),
        _ => bail!(
            "{} is right under /, so the workspace would be all of /; \
             move it into a folder of its own",
            repo.display()
        ),
    };
    let home = home.map(|h| std::fs::canonicalize(h).unwrap_or_else(|_| h.to_path_buf()));
    if home.as_deref() == Some(workspace.as_path()) {
        bail!(
            "{} is right in your home folder, so the workspace would be all of it; \
             move it into a folder of its own",
            repo.display()
        );
    }
    Ok(Some(workspace))
}

fn graph(entry: String, workspace: Option<PathBuf>) -> Result<()> {
    let (entry, workspace) = entry_and_workspace(&entry, workspace)?;
    let workspace = fghj::resolve_workspace(entry, Some(workspace), None)?;
    let g = fghj::resolver::resolve_universe(&workspace)?;
    println!("{}", serde_json::to_string_pretty(&g)?);
    Ok(())
}

fn probe_daemon() -> bool {
    UnixStream::connect(fghj::daemon::socket_path()).is_ok()
}

/// Hand-rolled: tiny bodies, no keep-alive, no HTTP client dependency.
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

    let (entry, absolute_workspace) = entry_and_workspace(&entry, workspace)?;

    // Sent so root `fghjd` can clone as this user.
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

/// Toggles whether `fghjd` is active; never touches the process itself.
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
    println!(
        "fghjd is active — serving 80/443 on {} and answering *.fghj.internal DNS",
        fghj::web::proxy::PROXY_IP
    );
    Ok(())
}

fn daemon_stop() -> Result<()> {
    if !probe_daemon() {
        println!("fghjd is not running");
        return Ok(());
    }
    http_post_json("/daemon/stop", &serde_json::json!({}))?;
    println!(
        "fghjd is now idle — 80/443, the loopback alias, DNS, and /etc/hosts released (fghjd itself is still running; \
         `fghj daemon start` to reconcile again)"
    );
    Ok(())
}

/// Only an explicit `y`/`yes`: the delete can't be undone.
fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Needs root, and won't re-exec under `sudo`: silently escalating to delete a
/// trusted root is the wrong shape.
fn uninstall(yes: bool, keep_ca: bool, keep_state: bool) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!(
            "fghj uninstall needs root — it removes a certificate from {}, \
             {}, and fghjd's system configuration.\n\nRun: sudo fghj uninstall",
            fghj::web::trust::store_name(),
            fghj::persistence::fghjd_root().display()
        );
    }

    if !yes {
        println!("This will remove:");
        println!("  - the fghjd-managed block in /etc/hosts, /etc/resolver/fghj*, the lo0");
        println!("    aliases and the pf anchor (stopping fghjd if it is running)");
        if !keep_ca {
            println!(
                "  - the \"{}\" root certificate from {}",
                fghj::web::ca::COMMON_NAME,
                fghj::web::trust::store_name()
            );
        }
        if !keep_state {
            println!(
                "  - {} — the CA private key, every wired workspace, and all run state",
                fghj::persistence::fghjd_root().display()
            );
        }
        println!();
        println!("Docker containers, networks and volumes are NOT removed (a volume may");
        println!("hold data you still want). Neither are the binaries — use");
        println!("`brew uninstall fghj` for those.");
        print!("\nContinue? [y/N] ");
        std::io::stdout().flush().ok();

        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if !confirmed(&answer) {
            println!("Aborted; nothing was removed.");
            return Ok(());
        }
    }

    let outcome = fghj::uninstall::purge(
        fghj::uninstall::Options {
            keep_ca,
            keep_state,
        },
        || {
            if !probe_daemon() {
                return Ok(false);
            }
            http_post_json("/daemon/stop", &serde_json::json!({}))?;
            Ok(true)
        },
    );

    if outcome.daemon_was_running {
        println!("stopped fghjd and unwound its system configuration");
    } else {
        println!("fghjd was not running; cleared any system configuration it left behind");
    }
    match outcome.certificates_removed {
        0 if keep_ca => println!("kept the root CA trusted (--keep-ca)"),
        0 => println!(
            "no \"{}\" certificate was trusted",
            fghj::web::ca::COMMON_NAME
        ),
        n => println!(
            "removed {n} \"{}\" certificate(s) from {}",
            fghj::web::ca::COMMON_NAME,
            fghj::web::trust::store_name()
        ),
    }
    match &outcome.state_dir_removed {
        Some(path) => println!("removed {}", path.display()),
        None if keep_state => println!(
            "kept {} (--keep-state)",
            fghj::persistence::fghjd_root().display()
        ),
        None => println!(
            "{} did not exist",
            fghj::persistence::fghjd_root().display()
        ),
    }

    for warning in &outcome.warnings {
        eprintln!("warning: {warning}");
    }

    println!();
    println!("Remaining: the binaries (`brew uninstall fghj`) and any fghj- Docker");
    println!("resources (`docker ps -a --filter name=^fghj-`).");
    Ok(())
}

fn daemon_restart() -> Result<()> {
    daemon_stop()?;
    daemon_start()
}

/// Ctrl-C/Ctrl-Z go to the remote process, as with `docker exec -it`.
/// Restored on drop.
struct RawModeGuard {
    fd: i32,
    original: libc::termios,
}

impl RawModeGuard {
    fn enable(fd: i32) -> Result<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            bail!("tcgetattr failed: {}", std::io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            bail!("tcsetattr failed: {}", std::io::Error::last_os_error());
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

/// 80x24 when stdout isn't a terminal, which only happens with `tty` off.
fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// TTY when both stdin and stdout are terminals, like `docker compose exec`;
/// `-T` forces it off.
async fn exec_cmd(
    node: String,
    workspace: Option<PathBuf>,
    no_tty: bool,
    cmd: Vec<String>,
) -> Result<()> {
    if !probe_daemon() {
        bail!(
            "fghjd isn't running — start it with `sudo fghjd` in another terminal \
             (or install it as a system service; see SPEC.md)"
        );
    }

    let canonical = workspace_root(workspace)?;

    let workspaces = http_get_json("/workspaces")?;
    let id = workspaces
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| {
            w["workspace"]
                .as_str()
                .map(PathBuf::from)
                .is_some_and(|p| p == canonical)
        })
        .and_then(|w| w["id"].as_str())
        .with_context(|| {
            format!(
                "{} isn't wired into fghjd — run `fghj wire` first",
                canonical.display()
            )
        })?
        .to_string();

    let tty = !no_tty && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (cols, rows) = if tty { terminal_size() } else { (80, 24) };

    let socket_path = fghj::daemon::socket_path();
    let unix_stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .with_context(|| {
            format!(
                "failed to connect to fghjd's control socket at {} — is fghjd running?",
                socket_path.display()
            )
        })?;

    let url = format!("ws://fghjd/nodes/{node}/exec/ws?workspace={id}");
    let (ws_stream, _response) = tokio_tungstenite::client_async(url, unix_stream)
        .await
        .context("failed to open exec websocket with fghjd")?;
    let (mut write, mut read) = ws_stream.split();

    write
        .send(WsMessage::Text(
            serde_json::json!({
                "cmd": cmd,
                "tty": tty,
                "cols": cols,
                "rows": rows,
            })
            .to_string()
            .into(),
        ))
        .await
        .context("failed to send exec start message")?;

    let _raw_guard = if tty {
        Some(RawModeGuard::enable(libc::STDIN_FILENO)?)
    } else {
        None
    };
    let mut resize_signal = if tty {
        Some(
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                .context("failed to install SIGWINCH handler")?,
        )
    } else {
        None
    };

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 8192];
    let mut stdin_eof = false;
    let mut exit_code = -1i32;
    let mut failure: Option<String> = None;

    loop {
        // `select!` still builds a disabled branch's future, so `unwrap()` on `None`
        // would panic; resolve to `pending()` instead.
        let resize_wait = async {
            match resize_signal.as_mut() {
                Some(sig) => {
                    sig.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            n = stdin.read(&mut buf), if !stdin_eof => {
                match n {
                    Ok(0) => {
                        stdin_eof = true;
                        // An empty binary frame means stdin EOF.
                        if write.send(WsMessage::Binary(Vec::new().into())).await.is_err() {
                            break;
                        }
                    }
                    Ok(n) => {
                        if write.send(WsMessage::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            _ = resize_wait => {
                let (cols, rows) = terminal_size();
                let _ = write
                    .send(WsMessage::Text(
                        serde_json::json!({ "type": "resize", "cols": cols, "rows": rows })
                            .to_string()
                            .into(),
                    ))
                    .await;
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Binary(bytes))) => {
                        if stdout.write_all(&bytes).await.is_err() {
                            break;
                        }
                        let _ = stdout.flush().await;
                    }
                    Some(Ok(WsMessage::Text(text))) => {
                        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                        match v["type"].as_str() {
                            Some("exit") => {
                                exit_code = v["code"].as_i64().unwrap_or(-1) as i32;
                                break;
                            }
                            Some("error") => {
                                failure = Some(
                                    v["message"].as_str().unwrap_or("unknown error").to_string(),
                                );
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    drop(_raw_guard);
    if let Some(message) = failure {
        bail!("exec failed: {message}");
    }
    std::process::exit(exit_code);
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

/// A mark, not a color, so it survives a pipe.
fn print_check(check: &fghj::doctor::Check) {
    let mark = match check.verdict {
        Verdict::Pass => "\u{2713}",
        Verdict::Warn => "!",
        Verdict::Fail => "\u{2717}",
    };
    println!("{mark} {}: {}", check.title, check.detail);
    if let Some(hint) = &check.hint {
        println!("    \u{2192} {hint}");
    }
}

/// Client checks first: when the socket is what's broken, that's the answer.
fn doctor() -> Result<()> {
    let client = fghj::doctor::client_checks();
    for check in &client {
        print_check(check);
    }

    if !probe_daemon() {
        println!();
        bail!(
            "fghjd isn't running, so the rest of the checks (Docker, ports, DNS, CA) \
             can't run — start it with `sudo fghjd`"
        );
    }

    // An old `fghjd` serves the UI for unknown routes, so the error is "not JSON".
    let resp = http_get_json("/daemon/doctor").context(
        "could not get a doctor report from fghjd — if the response above is HTML, the \
         running fghjd is older than this CLI and has no doctor endpoint; restart it on \
         the current build",
    )?;
    let report: fghj::doctor::Report = serde_json::from_value(resp)
        .context("fghjd returned a doctor report this version of fghj can't read")?;
    for check in &report.checks {
        print_check(check);
    }

    let failures = report
        .checks
        .iter()
        .chain(client.iter())
        .filter(|c| c.verdict == Verdict::Fail)
        .count();
    println!();
    if failures == 0 {
        println!("all clear");
        return Ok(());
    }
    // The failures were already printed with their hints.
    println!("{failures} check(s) failed");
    std::process::exit(1);
}

// Only `exec` needs async.
#[tokio::main]
async fn main() -> Result<()> {
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
        Commands::Doctor => doctor(),
        Commands::Uninstall {
            yes,
            keep_ca,
            keep_state,
        } => uninstall(yes, keep_ca, keep_state),
        Commands::Exec {
            node,
            workspace,
            no_tty,
            cmd,
        } => exec_cmd(node, workspace, no_tty, cmd).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_yes_confirms_an_irreversible_uninstall() {
        for yes in ["y", "Y", "yes", "YES", " yes \n", "Yes\n"] {
            assert!(confirmed(yes), "{yes:?} should confirm");
        }
        for no in ["", "\n", "n", "no", "ye", "yep", "sure", "q", "-y"] {
            assert!(!confirmed(no), "{no:?} must not confirm");
        }
    }

    /// `<tmp>/home/ws/app/.fghj.yaml`, canonical, so `/var` and `/private/var`
    /// compare equal.
    fn workspace_with_a_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("home/ws/app")).unwrap();
        std::fs::write(root.join("home/ws/app/.fghj.yaml"), "version: 1\n").unwrap();
        (tmp, root)
    }

    #[test]
    fn a_repos_fghj_yaml_or_folder_wires_its_parent() {
        let (_tmp, root) = workspace_with_a_repo();
        let app = root.join("home/ws/app");
        let home = root.join("home");
        let ws = Some(root.join("home/ws"));

        assert_eq!(
            local_workspace(".fghj.yaml", &app, Some(&home)).unwrap(),
            ws
        );
        assert_eq!(local_workspace(".", &app, Some(&home)).unwrap(), ws);
        assert_eq!(
            local_workspace("app", &app.join(".."), Some(&home)).unwrap(),
            ws
        );
    }

    #[test]
    fn an_entry_not_on_disk_is_a_git_url() {
        let (_tmp, root) = workspace_with_a_repo();
        for url in [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app.git",
        ] {
            assert_eq!(local_workspace(url, &root, None).unwrap(), None);
        }
    }

    #[test]
    fn a_folder_without_fghj_yaml_or_another_file_is_refused() {
        let (_tmp, root) = workspace_with_a_repo();
        std::fs::write(root.join("home/ws/app/README.md"), "").unwrap();

        let no_yaml = local_workspace("ws", &root.join("home"), None).unwrap_err();
        assert!(
            no_yaml.to_string().contains("has no .fghj.yaml"),
            "{no_yaml}"
        );
        let other = local_workspace("README.md", &root.join("home/ws/app"), None).unwrap_err();
        assert!(other.to_string().contains("isn't a .fghj.yaml"), "{other}");
    }

    #[test]
    fn a_repo_right_in_home_is_refused() {
        let (_tmp, root) = workspace_with_a_repo();
        let home = root.join("home/ws");

        let err = local_workspace(".", &root.join("home/ws/app"), Some(&home)).unwrap_err();
        assert!(err.to_string().contains("home folder"), "{err}");
    }
}
