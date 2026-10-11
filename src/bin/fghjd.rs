use anyhow::{Result, bail};

/// Doesn't daemonize: run in the foreground or under launchd/systemd.
#[tokio::main]
async fn main() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("fghjd must run as root — invoke it via `sudo fghjd`");
    }

    fghj::daemon::run_control_api().await
}
