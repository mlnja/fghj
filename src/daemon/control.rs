use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::daemon::registry::WorkspaceRegistry;
use crate::web::{ca, proxy};
use crate::{daemon_log, dns, effects, hosts_file, persistence, raw_net, supervisor};

/// Aborting these frees the ports they hold.
pub(crate) struct ActiveResources {
    dns_task: tokio::task::AbortHandle,
    http_task: tokio::task::AbortHandle,
    https_task: tokio::task::AbortHandle,
    effect_tasks: effects::EffectTasks,
}

/// The on/off switch behind `fghj daemon start`/`stop`: releases 80/443, DNS
/// and `/etc/hosts` while the process stays up.
pub struct DaemonControl {
    pub(crate) registry: Arc<WorkspaceRegistry>,
    pub(crate) cert_resolver: Arc<ca::DynamicCertResolver>,
    pub(crate) provider: Arc<rustls::crypto::CryptoProvider>,
    pub(crate) control_port: u16,
    pub(crate) active: Mutex<Option<ActiveResources>>,
    /// Freshness of container status for `/daemon/net-status`. `None` until the
    /// first tick or while idle.
    pub(crate) last_reconcile_ms: Mutex<Option<u64>>,
    /// Why the reconciler couldn't bring a workspace's sidecar back, by workspace
    /// id, until it manages to. For the doctor.
    pub(crate) sidecar_failures: Mutex<HashMap<String, String>>,
    /// Durable: `daemon stop` should survive a crash or reboot until countermanded.
    /// Read once at construction, written through by `set_idle_requested`.
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

    /// A no-op if already active.
    pub async fn activate(&self) -> Result<()> {
        if self.is_active() {
            return Ok(());
        }

        let dns_socket = dns::bind().await?;
        let dns_port = dns_socket
            .local_addr()
            .context("DNS socket has no local address")?
            .port();
        let dns_task = supervisor::supervise_forever(
            "dns server",
            dns::serve(dns_socket, self.registry.clone(), None),
        );

        // Before the binds: macOS only assigns `127.0.0.1` to `lo0`. Not part of
        // `raw_net::reconcile`, since it must exist with no workspace wired at all.
        raw_net::add_loopback_alias(proxy::PROXY_IP).with_context(|| {
            format!(
                "failed to alias {} onto lo0 — fghjd needs it to bind its proxy",
                proxy::PROXY_IP
            )
        })?;

        let http_listener = proxy::bind_http().await?;
        let https_listener = proxy::bind_https().await?;
        let http_task = supervisor::supervise_forever(
            "http redirect listener (:80)",
            proxy::serve_http_redirect(http_listener, self.registry.clone()),
        );
        let https_task = supervisor::supervise_forever(
            "https proxy listener (:443)",
            proxy::serve_https(
                https_listener,
                self.cert_resolver.clone(),
                self.control_port,
                self.provider.clone(),
                self.registry.clone(),
            ),
        );

        let effect_tasks = effects::spawn_all(dns_port, self.registry.subscribe_actors());

        *self.active.lock().unwrap() = Some(ActiveResources {
            dns_task,
            http_task,
            https_task,
            effect_tasks,
        });
        Ok(())
    }

    pub fn last_reconcile_ms(&self) -> Option<u64> {
        *self.last_reconcile_ms.lock().unwrap()
    }

    /// Running containers are untouched, just unreachable until `activate`. A no-op
    /// if already idle.
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
        // `raw_net::clear` won't: `PROXY_IP` is outside the raw-zone pool on purpose.
        if let Err(e) = raw_net::remove_loopback_alias(proxy::PROXY_IP) {
            daemon_log::warn(format!(
                "fghjd: failed to remove the {} lo0 alias on deactivate: {e}",
                proxy::PROXY_IP
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
            sidecar_failures: Mutex::new(HashMap::new()),
            idle_requested: Mutex::new(idle_requested),
            daemon_state_path: state_path,
        })
    }

    #[tokio::test]
    async fn idle_requested_is_cached_in_memory_and_written_through_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("daemon-state.json");
        let daemon = test_daemon_control(state_path.clone()).await;

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

        // `idle_requested` is read once at boot, not live.
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
