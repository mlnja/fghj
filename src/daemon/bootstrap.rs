use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::daemon::control::DaemonControl;
use crate::daemon::reconcile::{spawn_reconciler, spawn_sync_reconciler};
use crate::daemon::registry::WorkspaceRegistry;
use crate::daemon::{ca_dir, certs_dir, socket_path};
use crate::web::api::build_router;
use crate::web::{ca, trust};
use crate::{daemon_log, dns, persistence, supervisor};

/// Falls back to the socket of the `docker` CLI's active context: Docker
/// Desktop, OrbStack and colima don't use the default socket.
pub fn connect_docker() -> Result<bollard::Docker> {
    match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => Ok(docker),
        Err(default_err) => {
            let context_host = Command::new("docker")
                .args([
                    "context",
                    "inspect",
                    "--format",
                    "{{.Endpoints.docker.Host}}",
                ])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|h| !h.is_empty());
            match context_host {
                Some(host) => {
                    bollard::Docker::connect_with_socket(&host, 120, bollard::API_DEFAULT_VERSION)
                        .with_context(|| {
                            format!("failed to connect to the docker context's socket ({host})")
                        })
                }
                None => Err(default_err).context("failed to construct a Docker client"),
            }
        }
    }
}

/// Runs until SIGTERM/SIGINT, deactivating first. `fghj daemon stop`/`start`
/// toggle the active state without ending the process.
pub async fn run_control_api() -> Result<()> {
    // Docker trouble never stops the daemon: the UI's doctor reports it.
    let docker = match connect_docker() {
        Ok(docker) => docker,
        Err(e) => {
            daemon_log::warn(format!(
                "fghjd: no Docker client, restart once Docker is up: {e:#}"
            ));
            crate::docker::undialled_client()
        }
    };
    if let Err(e) = docker.ping().await {
        daemon_log::warn(format!("fghjd: Docker is not answering: {e}"));
    }
    let docker = Arc::new(docker);

    // Only the HTTPS proxy dials this, for the zone apex. Bound first so the
    // proxy has something to relay to even if activation fails.
    let control_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("failed to bind control API")?;
    let control_port = control_listener
        .local_addr()
        .context("control API socket has no local address")?
        .port();

    // The CLI's channel. Opened up beyond root-only, since `fghj` runs as the user.
    let socket_path = socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let cli_listener = tokio::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind control socket {}", socket_path.display()))?;
    std::fs::set_permissions(
        &socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )
    .with_context(|| format!("failed to set permissions on {}", socket_path.display()))?;

    let cert_path = ca::ca_cert_path(&ca_dir());
    let ca = {
        let dir = ca_dir();
        tokio::task::spawn_blocking(move || ca::ensure_ca(&dir))
            .await
            .context("CA setup task panicked")??
    };
    ca::refresh_trust_files(&ca_dir(), &ca).context("failed to refresh CA trust files")?;

    // A directory holding only the trust files, so it can be mounted without
    // `ca-key.pem` beside it. Created up front: a missing bind source makes Docker
    // invent an empty directory.
    let certs = certs_dir();
    std::fs::create_dir_all(&certs)
        .with_context(|| format!("failed to create {}", certs.display()))?;
    ca::refresh_trust_files(&certs, &ca).context("failed to refresh container trust files")?;

    let signing_ca = {
        let dir = ca_dir();
        tokio::task::spawn_blocking(move || ca::ensure_signing_ca(&dir, &ca))
            .await
            .context("signing CA setup task panicked")??
    };

    // Rewritten at startup so an install that once copied the root key here is
    // scrubbed without waiting for a run. Best-effort: `ensure_sidecar` repeats it.
    if let Err(e) =
        tokio::task::spawn_blocking(crate::orchestrator::route_table::refresh_sidecar_ca_copy)
            .await
            .context("sidecar CA refresh task panicked")?
    {
        daemon_log::warn(format!(
            "fghjd: failed to refresh the sidecar CA copy: {e:#}"
        ));
    }

    // Last: it can fail outside fghj's control (no session to prompt in). The
    // doctor's `ca-trust` check reports it.
    if let Err(e) = tokio::task::spawn_blocking(move || trust::install(&cert_path))
        .await
        .context("CA trust install task panicked")?
    {
        daemon_log::warn(format!("fghjd: failed to trust the root CA: {e:#}"));
    }

    // Best-effort prefetch, so the first run doesn't pay for the pull or build;
    // `ensure_sidecar` surfaces a real error if it's needed.
    {
        let docker = docker.clone();
        supervisor::supervise("sidecar image prefetch", async move {
            if let Err(e) = crate::sidecar_image::ensure_built(&docker).await {
                daemon_log::warn(format!(
                    "fghjd: failed to pre-fetch the sidecar proxy image: {e:#}"
                ));
            }
        });
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());

    // Before the cert resolver, which gates reserved-TLD alias certs on routes.
    let registry = Arc::new(WorkspaceRegistry::load(docker).await);

    let cert_resolver = Arc::new(ca::DynamicCertResolver::new(
        signing_ca,
        provider.clone(),
        registry.clone(),
    ));

    let daemon_state_path = persistence::default_state_path();
    let idle_requested = persistence::load_daemon_state(&daemon_state_path).idle_requested;
    let daemon = Arc::new(DaemonControl {
        registry: registry.clone(),
        cert_resolver,
        provider,
        control_port,
        active: Mutex::new(None),
        last_reconcile_ms: Mutex::new(None),
        idle_requested: Mutex::new(idle_requested),
        daemon_state_path,
    });
    spawn_reconciler(Arc::clone(&daemon));
    spawn_sync_reconciler(Arc::clone(&daemon));

    // Stays idle if the last `fghj daemon` call was `stop`, so a crash or reboot
    // doesn't undo it.
    if daemon.is_idle_requested() {
        daemon_log::info(
            "fghjd: starting idle — last `fghj daemon` action was `stop`; run `fghj daemon start` to reconcile"
                .to_string(),
        );
    } else {
        daemon.activate().await?;
    }

    let app = build_router(registry, Arc::clone(&daemon));
    daemon_log::info(format!(
        "fghjd: control API listening on {} (CLI) and reachable via https://{}",
        socket_path.display(),
        dns::ZONE
    ));

    let cli_app = app.clone();
    supervisor::supervise_forever("control socket server", async move {
        if let Err(e) = axum::serve(cli_listener, cli_app).await {
            daemon_log::warn(format!("fghjd: control socket server error: {e}"));
        }
    });

    let shutdown_signal = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    };

    tokio::select! {
        result = axum::serve(control_listener, app) => {
            result.context("control API server error")?;
        }
        _ = shutdown_signal => {
            daemon_log::info(
                "fghjd: received shutdown signal, releasing ports and cleaning up...".to_string(),
            );
            daemon.deactivate();
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}
