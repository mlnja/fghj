use anyhow::{Result, bail};

/// Doesn't daemonize: run in the foreground or under launchd/systemd.
#[tokio::main]
async fn main() -> Result<()> {
    // Before the root check, so packaging and bug reports can ask any user.
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("fghjd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if unsafe { libc::geteuid() } != 0 {
        bail!("fghjd must run as root — invoke it via `sudo fghjd`");
    }

    fghj::daemon::run_control_api().await
}
