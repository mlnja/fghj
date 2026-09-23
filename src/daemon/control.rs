//! [`DaemonControl`] — the daemon's own lifecycle: the resources that
//! exist only while it is active, and the durable idle flag.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::daemon::registry::WorkspaceRegistry;
use crate::web::{ca, proxy};
use crate::{daemon_log, dns, effects, hosts_file, persistence, raw_net};

/// The pieces of `fghjd` that only exist while it's in the "active" state:
/// the DNS server, and the HTTP/HTTPS reverse proxy occupying 80/443.
/// Dropping (aborting) these tasks frees the ports/socket they held.
pub(crate) struct ActiveResources {
    dns_task: tokio::task::JoinHandle<()>,
    http_task: tokio::task::JoinHandle<()>,
    https_task: tokio::task::JoinHandle<()>,
    /// Drives `effects::raw_net::RawNetEffect`, `effects::dns::DnsEffect`,
    /// and `effects::hosts::HostsEffect` for as long as `fghjd` is active —
    /// the sole remaining callers of `raw_net::reconcile`,
    /// `dns::install_os_resolver_config`, and `hosts_file::sync` (see
    /// `spawn_reconciler`'s doc for why that function no longer calls any of
    /// them directly). Aborted on `deactivate`, so an idle `fghjd` never
    /// keeps converging pf routes, `/etc/resolver`, or `/etc/hosts`.
    effect_tasks: effects::EffectTasks,
}

/// `fghjd` itself is meant to run forever — started at boot and restarted on
/// crash by the OS service manager (launchd/systemd) — but the user still
/// needs a way to tell it to get out of the way without stopping the whole
/// process: release ports 80/443, stop answering `*.fghj.internal` DNS
/// queries, and drop every `#AdditionalHost` entry from `/etc/hosts`, while
/// staying alive and reachable so a later `fghj daemon start` can reconcile
/// everything back. `DaemonControl` is that on/off switch: the control API
/// (always up) holds one of these and toggles `active` in response to
/// `/daemon/start` and `/daemon/stop`.
pub struct DaemonControl {
    pub(crate) registry: Arc<WorkspaceRegistry>,
    pub(crate) cert_resolver: Arc<ca::DynamicCertResolver>,
    pub(crate) provider: Arc<rustls::crypto::CryptoProvider>,
    pub(crate) control_port: u16,
    pub(crate) active: Mutex<Option<ActiveResources>>,
    /// Epoch-millis timestamp of `spawn_reconciler`'s last completed
    /// container-status refresh tick while active — surfaced by
    /// `/daemon/net-status` so the telemetry drawer can show how fresh that
    /// data is. `/etc/hosts`/`/etc/resolver`/raw-net routes are no longer
    /// synced on this tick (see `spawn_reconciler`'s doc) — they converge
    /// continuously via `effects::spawn_all`'s fanned-in effects instead, so
    /// this field no longer reflects their freshness. `None` until the
    /// first tick after `fghjd` starts (or while idle).
    pub(crate) last_reconcile_ms: Mutex<Option<u64>>,
    /// Tracks that the operator's last explicit `fghj daemon` call was
    /// `stop`, not just that `fghjd` currently happens to be idle in memory.
    /// Read once at construction from `persistence::DaemonState` at
    /// `persistence::default_state_path()` — next to the CA (durable,
    /// survives a reboot) rather than under `/var/run` — then kept in memory
    /// and written through on every change via `set_idle_requested`; nothing
    /// else reads or writes `daemon-state.json` directly. "I told it to
    /// stop" is a standing instruction that should hold until countermanded
    /// by `fghj daemon start`, not something a crash or a reboot should
    /// silently discard by reactivating anyway. The path itself is a field
    /// (defaulting to `persistence::default_state_path()` in production)
    /// rather than hardcoded in `set_idle_requested`, so tests can point it
    /// at a temp file instead of the real, root-owned, production path.
    pub(crate) idle_requested: Mutex<bool>,
    pub(crate) daemon_state_path: PathBuf,
}

impl DaemonControl {
    pub fn is_active(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    pub fn is_idle_requested(&self) -> bool {
        *self.idle_requested.lock().unwrap()
    }

    pub fn set_idle_requested(&self, idle_requested: bool) -> Result<()> {
        let path = &self.daemon_state_path;
        let mut state = persistence::load_daemon_state(path);
        state.idle_requested = idle_requested;
        persistence::save_daemon_state(path, &state)?;
        *self.idle_requested.lock().unwrap() = idle_requested;
        Ok(())
    }

    /// Binds the DNS server and the HTTP/HTTPS proxy, installs the OS
    /// resolver config, and syncs `/etc/hosts` — i.e. makes `fghjd` actually
    /// reachable at `*.fghj.internal` (and any declared additional hosts).
    /// A no-op if already active, so it's safe to call from `fghj daemon
    /// start` unconditionally without checking status first.
    pub async fn activate(&self) -> Result<()> {
        if self.is_active() {
            return Ok(());
        }

        let dns_socket = dns::bind().await?;
        let dns_port = dns_socket
            .local_addr()
            .context("DNS socket has no local address")?
            .port();
        let dns_task = tokio::spawn(dns::serve(dns_socket, self.registry.clone(), None));

        let http_listener = proxy::bind_http().await?;
        let https_listener = proxy::bind_https().await?;
        let http_task = tokio::spawn(proxy::serve_http_redirect(
            http_listener,
            self.registry.clone(),
        ));
        let https_task = tokio::spawn(proxy::serve_https(
            https_listener,
            self.cert_resolver.clone(),
            self.control_port,
            self.provider.clone(),
            self.registry.clone(),
        ));

        let effect_tasks = effects::spawn_all(dns_port, self.registry.actors().subscribe());

        *self.active.lock().unwrap() = Some(ActiveResources {
            dns_task,
            http_task,
            https_task,
            effect_tasks,
        });
        Ok(())
    }

    /// Epoch-millis of `spawn_reconciler`'s last completed tick, or
    /// `None` if it hasn't run yet. See the field doc for what this does and
    /// doesn't cover.
    pub fn last_reconcile_ms(&self) -> Option<u64> {
        *self.last_reconcile_ms.lock().unwrap()
    }

    /// Reverses `activate`: aborts the DNS/HTTP/HTTPS tasks (freeing the
    /// ports/socket they held) and clears fghj's managed entries from the OS
    /// resolver config and `/etc/hosts`. Docker containers already running
    /// are untouched — they keep running under Docker's own supervision and
    /// are simply unreachable until the next `activate`. A no-op if already
    /// idle.
    pub fn deactivate(&self) {
        if let Some(resources) = self.active.lock().unwrap().take() {
            resources.dns_task.abort();
            resources.http_task.abort();
            resources.https_task.abort();
            resources.effect_tasks.abort_all();
        }
        dns::clear_os_resolver_config();
        if let Err(e) = hosts_file::sync(&hosts_file::hosts_path(), &[]) {
            daemon_log::warn(format!(
                "fghjd: failed to clear /etc/hosts on deactivate: {e}"
            ));
        }
        if let Err(e) = raw_net::clear() {
            daemon_log::warn(format!(
                "fghjd: failed to clear raw-net routes on deactivate: {e}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::registry::tests::test_docker;

    async fn test_daemon_control(state_path: PathBuf) -> Arc<DaemonControl> {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Arc::new(
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await,
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert_resolver = Arc::new(ca::DynamicCertResolver::new(
            ca::generate_ca_for_tests(),
            provider.clone(),
            registry.clone(),
        ));
        let idle_requested = persistence::load_daemon_state(&state_path).idle_requested;
        Arc::new(DaemonControl {
            registry,
            cert_resolver,
            provider,
            control_port: 0,
            active: Mutex::new(None),
            last_reconcile_ms: Mutex::new(None),
            idle_requested: Mutex::new(idle_requested),
            daemon_state_path: state_path,
        })
    }

    #[tokio::test]
    async fn idle_requested_is_cached_in_memory_and_written_through_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("daemon-state.json");
        let daemon = test_daemon_control(state_path.clone()).await;

        // Read once at construction: a freshly-started daemon with no prior
        // `daemon stop` comes back active, matching the on-disk default.
        assert!(!daemon.is_idle_requested());

        daemon.set_idle_requested(true).unwrap();
        assert!(
            daemon.is_idle_requested(),
            "in-memory cache must reflect the change immediately"
        );
        assert!(
            persistence::load_daemon_state(&state_path).idle_requested,
            "the change must be written through to disk, not just cached in memory"
        );

        // Mutating the file directly (simulating some other process) must
        // NOT be observed without going through `set_idle_requested` —
        // `idle_requested` is read once at boot, not re-read live.
        persistence::save_daemon_state(
            &state_path,
            &persistence::DaemonState {
                idle_requested: false,
            },
        )
        .unwrap();
        assert!(
            daemon.is_idle_requested(),
            "the in-memory cache must not silently pick up an out-of-band disk change"
        );
    }
}
