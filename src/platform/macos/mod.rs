//! macOS: `/etc/resolver`, the System keychain, `pf` and `lo0` aliases,
//! installed through Homebrew and run by launchd.

use std::process::Command;

use anyhow::{Context, Result, bail};

use super::Platform;

mod homebrew;
mod keychain;
mod lo0;
mod pf;
mod resolver_dir;
mod session;

pub(super) fn platform() -> Platform {
    Platform {
        dns: Box::new(resolver_dir::ResolverDir),
        trust: Box::new(keychain::SystemKeychain),
        nat: Box::new(pf::PfAnchor::default()),
        loopback: Box::new(lo0::Lo0),
        session: Box::new(session::MacSession),
        packaging: Box::new(homebrew::Homebrew),
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let output = cmd
        .output()
        .with_context(|| format!("failed to run {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
