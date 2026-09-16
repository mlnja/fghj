//! In-process ring buffer of `fghjd`'s own operational log lines.
//!
//! `fghjd` has no logging crate dependency and no persistent log file — its
//! `println!`/`eprintln!` call sites (`daemon.rs`, `dns.rs`, `raw_net`) are
//! its entire log output today, visible only to whatever's supervising the
//! process (a terminal, launchd/systemd's own log capture). Routing those
//! same call sites through [`info`]/[`warn`] additionally records them here,
//! so the control API can serve them back to the telemetry drawer's "Logs"
//! tab. This is deliberately just a capped in-memory buffer, reset on every
//! `fghjd` restart — not a durable audit log.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Caps memory use — at this size, even a busy `fghjd` takes a long time to
/// wrap around, while the buffer never grows unbounded across a long-lived
/// process.
const CAPACITY: usize = 2000;

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Monotonically increasing within this process — lets a poller ask for
    /// "everything after the last one I saw" (see [`tail`]) without needing
    /// timestamps to be strictly ordered or unique.
    pub seq: u64,
    pub ts_ms: u64,
    pub level: &'static str,
    pub message: String,
}

struct State {
    entries: Mutex<VecDeque<LogEntry>>,
    next_seq: AtomicU64,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| State {
        entries: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        next_seq: AtomicU64::new(1),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn push(level: &'static str, message: String) {
    let state = state();
    let seq = state.next_seq.fetch_add(1, Ordering::Relaxed);
    let mut entries = state.entries.lock().unwrap();
    if entries.len() == CAPACITY {
        entries.pop_front();
    }
    entries.push_back(LogEntry {
        seq,
        ts_ms: now_ms(),
        level,
        message,
    });
}

/// Records an informational message and prints it to stdout, exactly as a
/// bare `println!` would — the ring buffer is additive, never a replacement
/// for the terminal/service-manager log output an operator already relies
/// on.
pub fn info(message: impl Into<String>) {
    let message = message.into();
    println!("{message}");
    push("info", message);
}

/// Same as [`info`], but for messages that already went to stderr —
/// preserves that distinction in the ring buffer's `level` field.
pub fn warn(message: impl Into<String>) {
    let message = message.into();
    eprintln!("{message}");
    push("warn", message);
}

/// Entries after `after_seq` (or the most recent `limit` if `after_seq` is
/// `None`), oldest-first — lets a poller do an initial "give me what's
/// recent" load and then incremental "give me what's new since `seq`"
/// follow-ups with the same call.
pub fn tail(after_seq: Option<u64>, limit: usize) -> Vec<LogEntry> {
    let entries = state().entries.lock().unwrap();
    match after_seq {
        Some(after) => entries
            .iter()
            .filter(|e| e.seq > after)
            .take(limit)
            .cloned()
            .collect(),
        None => {
            let len = entries.len();
            entries
                .iter()
                .skip(len.saturating_sub(limit))
                .cloned()
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests share process-global state with every other test in this
    // binary (and, if `cargo test` runs threads in parallel, with each
    // other) — each test only asserts things about the specific markers it
    // itself pushed, never about the buffer being empty or a marker being
    // the globally-last entry, so they stay correct regardless of what else
    // is being logged concurrently.

    #[test]
    fn tail_without_after_seq_respects_the_limit_and_stays_ordered() {
        for i in 0..10 {
            info(format!("daemon_log test limit-marker {i}"));
        }
        let recent = tail(None, 3);
        assert_eq!(recent.len(), 3);
        assert!(recent.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[test]
    fn tail_after_seq_returns_only_newer_entries_in_order() {
        let baseline = tail(None, 1).last().map(|e| e.seq).unwrap_or(0);
        for i in 0..5 {
            info(format!("daemon_log test after-marker {i}"));
        }
        let newer = tail(Some(baseline), 10_000);
        let ours: Vec<&LogEntry> = newer
            .iter()
            .filter(|e| e.message.starts_with("daemon_log test after-marker "))
            .collect();
        assert_eq!(ours.len(), 5);
        assert!(ours.windows(2).all(|w| w[0].seq < w[1].seq));
        assert!(newer.iter().all(|e| e.seq > baseline));
    }

    #[test]
    fn warn_records_the_warn_level() {
        warn("daemon_log test warn-marker");
        let baseline_check = tail(None, 50);
        let found = baseline_check
            .iter()
            .rev()
            .find(|e| e.message == "daemon_log test warn-marker")
            .expect("just-pushed warn entry should be in the recent tail");
        assert_eq!(found.level, "warn");
    }
}
