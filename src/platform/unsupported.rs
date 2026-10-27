//! An OS fghj has no implementation for: the daemon still runs and serves the
//! container-side names, and every host integration says it isn't there
//! instead of failing. A platform that has some capabilities but not yet
//! others borrows the missing ones from here.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Result;

use super::{DnsRouting, Loopback, OsRouting, Packaging, Platform, RawNat, Session, TrustStore};
use crate::daemon_log;
use crate::raw_net::RouteSpec;

pub(super) fn platform() -> Platform {
    Platform {
        dns: Box::new(NoDnsRouting),
        trust: Box::new(NoTrustStore),
        nat: Box::new(NoRawNat::default()),
        loopback: Box::new(NoLoopback),
        session: Box::new(PlainSession),
        packaging: Box::new(NoPackaging),
    }
}

pub(super) struct NoDnsRouting;

impl DnsRouting for NoDnsRouting {
    fn sync(&self, port: u16, _zones: &[&str]) -> Result<()> {
        daemon_log::warn(format!(
            "fghjd: automatic OS DNS routing for *.{} isn't implemented on this platform yet — \
             point your resolver at 127.0.0.1:{port} for that zone manually",
            crate::dns::ZONE
        ));
        Ok(())
    }

    fn clear(&self) {}

    fn status(&self) -> Option<OsRouting> {
        None
    }

    fn footprint(&self) -> Option<&'static str> {
        None
    }
}

pub(super) struct NoTrustStore;

impl TrustStore for NoTrustStore {
    fn install(&self, ca_cert_path: &Path) -> Result<()> {
        eprintln!(
            "fghjd: automatic system trust installation isn't implemented on this platform yet — \
             trust {} manually so browsers accept fghj's issued certificates",
            ca_cert_path.display()
        );
        Ok(())
    }

    fn remove(&self) -> usize {
        0
    }

    fn is_trusted(&self, _ca_cert_path: &Path) -> Option<bool> {
        None
    }

    fn store_name(&self) -> &'static str {
        "the system trust store"
    }

    fn install_notice(&self) -> &'static str {
        "fghj's root CA isn't trusted yet"
    }

    fn untrusted_hint(&self) -> &'static str {
        "add the CA to your system trust store by hand; `fghj doctor --fix` can't on this \
         platform"
    }
}

#[derive(Default)]
pub(super) struct NoRawNat {
    warned: OnceLock<()>,
}

impl RawNat for NoRawNat {
    fn apply(&self, routes: &[RouteSpec]) -> Result<()> {
        if !routes.is_empty() {
            self.warned.get_or_init(|| {
                daemon_log::warn(
                    "fghjd: host-reachable raw ports (*.fghj.raw.internal) aren't implemented on \
                     this platform yet — raw-zone names will resolve to a virtual IP, but \
                     nothing will answer on it"
                        .to_string(),
                );
            });
        }
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        Ok(())
    }

    fn status(&self) -> Vec<RouteSpec> {
        Vec::new()
    }

    fn unserved(&self, _routes: &[RouteSpec]) -> Option<Result<Vec<RouteSpec>>> {
        None
    }

    fn name(&self) -> &'static str {
        "not implemented"
    }

    fn location(&self) -> &'static str {
        "nowhere"
    }

    fn inspect_command(&self) -> &'static str {
        "there is nothing to inspect on this platform"
    }

    fn footprint(&self) -> Option<&'static str> {
        None
    }
}

/// Elsewhere all of `127/8` already routes to loopback.
pub(super) struct NoLoopback;

impl Loopback for NoLoopback {
    fn add_alias(&self, _ip: Ipv4Addr) -> Result<()> {
        Ok(())
    }

    fn remove_alias(&self, _ip: Ipv4Addr) -> Result<()> {
        Ok(())
    }

    fn alias_present(&self, _ip: Ipv4Addr) -> Option<Result<bool>> {
        None
    }

    fn interface(&self) -> &'static str {
        "loopback"
    }
}

pub(super) struct PlainSession;

impl Session for PlainSession {
    fn state_root(&self) -> PathBuf {
        PathBuf::from("/var/lib/fghjd")
    }

    fn session_ssh_agent(&self, _uid: u32, _is_live: &dyn Fn(&Path) -> bool) -> Option<String> {
        None
    }

    fn is_session_agent(&self, _sock: &str) -> bool {
        false
    }
}

pub(super) struct NoPackaging;

impl Packaging for NoPackaging {
    fn start_service_hint(&self) -> &'static str {
        "however your system runs services"
    }

    fn uninstall_hint(&self) -> &'static str {
        "delete `fghj` and `fghjd` by hand"
    }

    fn install_cue_hint(&self) -> &'static str {
        "see https://cuelang.org/docs/install/"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_installs_and_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = tmp.path().join("ca.pem");
        NoTrustStore.install(&cert).unwrap();
        assert_eq!(NoTrustStore.remove(), 0);
        assert_eq!(NoTrustStore.is_trusted(&cert), None);
        assert_eq!(NoTrustStore.store_name(), "the system trust store");
    }

    #[test]
    fn raw_nat_accepts_routes_but_serves_none() {
        let nat = NoRawNat::default();
        let route = RouteSpec {
            virtual_ip: Ipv4Addr::new(10, 222, 1, 1),
            container_port: 9000,
            host_port: 54321,
        };
        assert!(nat.apply(&[]).is_ok());
        assert!(nat.apply(&[route]).is_ok());
        assert!(nat.clear().is_ok());
        assert!(nat.status().is_empty());
        assert!(nat.unserved(&[route]).is_none());
    }

    #[test]
    fn dns_routing_reports_unsupported() {
        assert_eq!(NoDnsRouting.status(), None);
    }
}
