use std::time::Duration;

use crate::docker;

/// Polls a container's declared healthcheck (if any) until it reports
/// "healthy", for up to two minutes — long enough for a real database's own
/// startup healthcheck, short enough that a genuinely broken one doesn't
/// hang a run forever. Returns as soon as there's nothing more to wait for:
/// no declared healthcheck, a terminal "unhealthy" report (best-effort —
/// fghj proceeds rather than blocking the run indefinitely), or the
/// container having vanished. Callers only call this at all when
/// `node.healthcheck.is_some()`, but it's written to be a safe no-op
/// otherwise too.
pub(crate) async fn wait_for_healthy(docker: &bollard::Docker, container_name: &str) {
    for _ in 0..60 {
        match docker::inspect_health(docker, container_name).await {
            Ok(Some(status)) if status == "healthy" || status == "unhealthy" => return,
            Ok(None) => return,
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
