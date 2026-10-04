use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
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
        /// Git URL of the entry repo to clone in, if not already in the workspace
        entry: String,
        /// Workspace root holding sibling repo checkouts (default: the wired
        /// workspace the current directory is inside, else the current directory)
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Wire a workspace into the running fghjd daemon so it shows up in the UI
    Wire {
        /// Git URL of the entry repo to clone in, if not already in the workspace
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
        /// Which run to target (default: the shared default environment)
        #[arg(long, default_value = "default")]
        run: String,
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

/// Turns the optional `--workspace` flag into an absolute, canonical path.
///
/// An explicit flag always wins. When it is absent we ask `fghjd` which
/// workspaces are wired and take the one the current directory sits inside,
/// so every workspace-scoped command works from anywhere in a workspace —
/// including several levels down inside one of its repos — instead of only
/// from the workspace root itself.
///
/// Canonical because that is what makes the comparison in
/// `enclosing_workspace` meaningful, and because `wire` registers the path
/// that `exec` later has to match exactly. Resolving both ends the same way
/// here is what keeps `/var` and `/private/var` from being two different
/// workspaces.
///
/// `canonicalize` failing is not fatal: `graph` is allowed to name a
/// workspace directory that doesn't exist yet (it creates it), so we fall
/// back to the merely-absolute path rather than erroring.
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
        // Announced on stderr, not stdout: `fghj graph` pipes JSON to
        // stdout, and silently operating on a directory the user never
        // named is exactly the kind of helpfulness that needs to be
        // visible. Suppressed when it derived the directory you were
        // already standing in, which tells you nothing.
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

/// The wired workspace containing `cwd`, according to `fghjd`.
///
/// Goes through the control API rather than reading `workspaces.json`
/// directly because that file lives under root-only `/var/lib/fghjd` and
/// this is the unprivileged CLI.
///
/// Every failure is `None`, never an error. The daemon may legitimately be
/// down — `fghj graph` resolves a workspace without one at all — and an
/// unreachable daemon should degrade to the old "current directory"
/// behaviour, not turn a working command into a failing one.
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

fn graph(entry: String, workspace: Option<PathBuf>) -> Result<()> {
    let workspace = fghj::resolve_workspace(Some(entry), Some(workspace_root(workspace)?), None)?;
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

    let absolute_workspace = workspace_root(workspace)?;

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
/// serving 80/443 on its own loopback alias, `*.fghj.internal` DNS, and
/// `/etc/hosts`, or sitting
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

/// Reads a yes/no answer from stdin. Requires a full `y`/`yes` — anything
/// else, including a bare Enter or a closed stdin (piped, CI), is "no".
/// Defaulting an irreversible delete to "no" is the only safe direction.
fn confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// `fghj uninstall` — the counterpart to the Homebrew caveats that used to
/// just *print* these steps and leave them to the user.
///
/// Needs root for all three of its jobs (the System keychain, `/etc/*`, and
/// `/var/lib/fghjd`), and deliberately does not re-exec itself under `sudo`:
/// a command that silently escalates to delete a trusted root certificate is
/// the wrong shape. It refuses and tells the user what to run instead.
fn uninstall(yes: bool, keep_ca: bool, keep_state: bool) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!(
            "fghj uninstall needs root — it removes a certificate from the System keychain, \
             {}, and fghjd's system configuration.\n\nRun: sudo fghj uninstall",
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
                fghj::web::ca::SYSTEM_KEYCHAIN
            );
        }
        if !keep_state {
            println!(
                "  - {} — the CA private key, every wired workspace, and all run state",
                fghj::persistence::fghjd_root().display()
            );
        }
        println!();
        println!("Docker containers, networks and volumes are NOT removed (a `scope: stable`");
        println!("volume may hold data you still want). Neither are the binaries — use");
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
            "removed {n} \"{}\" certificate(s) from the System keychain",
            fghj::web::ca::COMMON_NAME
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

/// Puts `fd` (expected to be `STDIN_FILENO`) into raw mode for the lifetime
/// of the guard — no line buffering, no local echo, no signal generation
/// from Ctrl-C/Ctrl-Z (so those bytes forward to the *remote* process
/// instead, matching `docker exec -it`). Restores the original termios on
/// drop, covering early return and error paths alike.
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

/// The local terminal's current size, in columns/rows — falls back to 80x24
/// if stdout isn't actually a terminal (e.g. `TIOCGWINSZ` fails), which is
/// harmless since that only happens when `tty` is already false.
fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// `fghj exec` — proxies a real `docker exec` full duplex over `fghjd`'s
/// control socket (see `daemon::get_run_exec_ws`/`docker::ExecSession`).
/// TTY allocation auto-detects the same way `docker compose exec` does: on
/// when both local stdin and stdout are real terminals, off (plain
/// bidirectional byte relay, no local raw mode/resize forwarding) otherwise
/// — `-T` forces it off even in a real terminal.
async fn exec_cmd(
    node: String,
    workspace: Option<PathBuf>,
    run: String,
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

    let url = format!("ws://fghjd/runs/{run}/nodes/{node}/exec/ws?workspace={id}");
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

    // Only constructed (and only ever restored) when a TTY was actually
    // allocated — leaving the local terminal in cooked mode for a plain,
    // non-interactive command.
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
        // `select!`'s `if` precondition only skips *polling* a branch — the
        // async expression that builds its future is still evaluated even
        // when disabled, so `resize_signal.as_mut().unwrap()` would panic
        // here on every non-TTY `exec` the moment `resize_signal` is `None`.
        // Routing through a future that resolves to `pending()` in that case
        // sidesteps the unwrap entirely and needs no precondition.
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
                        // Empty binary frame is the client's "stdin EOF"
                        // signal — see `daemon::handle_exec_socket`.
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

// Only `exec` is genuinely async (it needs a live duplex WebSocket) — every
// other subcommand is plain blocking I/O, called directly from here same as
// before `main` grew a runtime.
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
        Commands::Uninstall {
            yes,
            keep_ca,
            keep_state,
        } => uninstall(yes, keep_ca, keep_state),
        Commands::Exec {
            node,
            workspace,
            run,
            no_tty,
            cmd,
        } => exec_cmd(node, workspace, run, no_tty, cmd).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the direction of the default on `fghj uninstall`'s prompt.
    /// The action it gates — deleting a trusted root CA and its private
    /// key — cannot be undone, so every answer that isn't an explicit yes
    /// has to read as no, including the empty string a bare Enter or a
    /// closed stdin produces.
    #[test]
    fn only_an_explicit_yes_confirms_an_irreversible_uninstall() {
        for yes in ["y", "Y", "yes", "YES", " yes \n", "Yes\n"] {
            assert!(confirmed(yes), "{yes:?} should confirm");
        }
        for no in ["", "\n", "n", "no", "ye", "yep", "sure", "q", "-y"] {
            assert!(!confirmed(no), "{no:?} must not confirm");
        }
    }
}
