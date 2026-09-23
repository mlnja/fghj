use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::StreamExt;

use crate::docker;
use crate::persistence::{LogLine, WorkspaceDb};
use crate::state::RunState;

/// Returns the container name backing `node_id` in `state`, for the SSE
/// live-follow endpoint to build a `docker::logs_follow` stream from.
pub fn container_name_for<'a>(state: &'a RunState, node_id: &str) -> Result<&'a str> {
    state
        .containers
        .get(node_id)
        .map(|c| c.desired.container_name.as_str())
        .ok_or_else(|| anyhow::anyhow!("no such node in run: {node_id}"))
}

/// Runs for the lifetime of one container: opens a fresh log generation,
/// then streams `container_name`'s stdout/stderr into it via
/// `docker::logs_follow` until the stream ends (the container stops or is
/// removed), flushing periodically so a live tail stays close to real-time
/// and unconditionally at the end so a crash's final lines are never lost —
/// the whole point of persisting logs in the first place, since Docker's own
/// retained logs are destroyed the moment `stop_and_remove` runs.
///
/// Each stream (stdout/stderr) gets its own leftover buffer so a line split
/// across two chunks is reassembled correctly without stdout/stderr
/// interleaving corrupting each other's partial line.
pub(crate) async fn capture_container_logs(
    db: Arc<WorkspaceDb>,
    docker: Arc<bollard::Docker>,
    run_id: String,
    node_id: String,
    container_name: String,
) {
    let generation = match db
        .clone()
        .begin_log_generation(run_id.clone(), node_id.clone())
        .await
    {
        Ok(g) => g,
        Err(e) => {
            eprintln!("fghjd: failed to begin log generation for node {node_id}: {e:#}");
            return;
        }
    };

    let mut stream = docker::logs_follow(&docker, &container_name, true);
    let mut seq: i64 = 0;
    let mut buffer: Vec<LogLine> = Vec::new();
    let mut leftover_out = String::new();
    let mut leftover_err = String::new();
    let mut last_flush = std::time::Instant::now();

    while let Some(item) = stream.next().await {
        let (is_err, message) = match item {
            Ok(bollard::container::LogOutput::StdOut { message }) => (false, message),
            Ok(bollard::container::LogOutput::Console { message }) => (false, message),
            Ok(bollard::container::LogOutput::StdErr { message }) => (true, message),
            Ok(bollard::container::LogOutput::StdIn { .. }) => continue,
            Err(_) => break,
        };
        let leftover = if is_err {
            &mut leftover_err
        } else {
            &mut leftover_out
        };
        leftover.push_str(&String::from_utf8_lossy(&message));
        while let Some(pos) = leftover.find('\n') {
            let raw_line: String = leftover.drain(..=pos).collect();
            let raw_line = raw_line.trim_end_matches('\n');
            let (ts, line) = raw_line.split_once(' ').unwrap_or(("", raw_line));
            buffer.push(LogLine {
                seq,
                stream: if is_err { "stderr" } else { "stdout" }.to_string(),
                ts: ts.to_string(),
                line: line.to_string(),
            });
            seq += 1;
        }
        if buffer.len() >= 200 || last_flush.elapsed() >= Duration::from_millis(500) {
            let batch = std::mem::take(&mut buffer);
            let _ = db
                .clone()
                .insert_log_lines(run_id.clone(), node_id.clone(), generation, batch)
                .await;
            last_flush = std::time::Instant::now();
        }
    }

    for (is_err, leftover) in [(false, &mut leftover_out), (true, &mut leftover_err)] {
        if !leftover.is_empty() {
            let raw_line = std::mem::take(leftover);
            let (ts, line) = raw_line.split_once(' ').unwrap_or(("", raw_line.as_str()));
            buffer.push(LogLine {
                seq,
                stream: if is_err { "stderr" } else { "stdout" }.to_string(),
                ts: ts.to_string(),
                line: line.to_string(),
            });
            seq += 1;
        }
    }

    if !buffer.is_empty() {
        let _ = db
            .insert_log_lines(run_id, node_id, generation, buffer)
            .await;
    }
}

pub async fn logs_for_tail(
    docker: &bollard::Docker,
    state: &RunState,
    node_id: &str,
    tail: usize,
) -> Result<String> {
    let Some(c) = state.containers.get(node_id) else {
        bail!("no such node in run: {node_id}");
    };
    docker::logs_tail(docker, &c.desired.container_name, tail).await
}
