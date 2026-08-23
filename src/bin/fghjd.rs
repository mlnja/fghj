use anyhow::{bail, Result};

/// The one `fghjd` daemon on the machine (Docker-style: `dockerd` has one
/// instance managing many containers; this has one instance managing many
/// workspaces). This binary doesn't daemonize itself: for local dev, run it
/// directly (`sudo fghjd`) and it stays in the foreground, logging to that
/// terminal; in a real install it's supervised by systemd (Linux) or a
/// launchd LaunchDaemon (macOS), which already handle backgrounding,
/// restart-on-crash, and log capture.
#[tokio::main]
async fn main() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("fghjd must run as root — invoke it via `sudo fghjd`");
    }

    fghj::daemon::write_pid(std::process::id())?;
    println!("fghjd listening on 127.0.0.1:{}", fghj::daemon::CONTROL_PORT);
    fghj::daemon::run_control_api().await
}
