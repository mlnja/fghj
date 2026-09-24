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
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

use crate::util::time::now_ms;

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

/// The buffer and its sequence counter live behind **one** lock, together,
/// on purpose.
///
/// They used to be a `Mutex<VecDeque<_>>` beside an `AtomicU64`, with `push`
/// allocating the seq before taking the lock. Two concurrent writers could
/// then take seq 5 and 6 and append them in the other order, leaving the
/// buffer holding `[.., 6, 5]`. Out-of-order display was the harmless half:
/// `tail(Some(after), ..)` filters on `e.seq > after`, so a poller that had
/// already seen 6 would drop 5 forever — a log line silently lost, which is
/// the one thing a log must not do. `fghjd` has several concurrent writers
/// (converge tasks, DNS, `raw_net`), so this was reachable in normal
/// operation, not just under test.
///
/// Allocating the seq under the same lock that does the append makes seq
/// order and insertion order the same thing by construction, rather than by
/// remembering to keep them that way.
struct Log {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
}

struct State {
    log: Mutex<Log>,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(|| State {
        log: Mutex::new(Log {
            entries: VecDeque::with_capacity(CAPACITY),
            next_seq: 1,
        }),
    })
}

fn push(level: &'static str, message: String) {
    let mut log = state().log.lock().unwrap();
    let seq = log.next_seq;
    log.next_seq += 1;
    if log.entries.len() == CAPACITY {
        log.entries.pop_front();
    }
    log.entries.push_back(LogEntry {
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
    let log = state().log.lock().unwrap();
    let entries = &log.entries;
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

    /// The invariant the whole `after_seq` protocol rests on: a poller that
    /// asks for everything after the highest seq it has seen must never be
    /// able to skip past an entry that is still to be appended. That only
    /// holds if seq order and buffer order are the same, which in turn only
    /// holds if the seq is allocated under the same lock as the append.
    ///
    /// Asserted globally rather than over this test's own markers, because
    /// it is a global property — and it stays true no matter what else in
    /// the binary is logging concurrently, which is the point.
    #[test]
    fn concurrent_writers_cannot_append_out_of_seq_order() {
        std::thread::scope(|scope| {
            for t in 0..8 {
                scope.spawn(move || {
                    for i in 0..50 {
                        info(format!("daemon_log test race-marker {t}-{i}"));
                    }
                });
            }
        });
        let all = tail(None, CAPACITY);
        assert!(
            all.windows(2).all(|w| w[0].seq < w[1].seq),
            "buffer order disagrees with seq order, so `tail(Some(seq), ..)` can drop entries"
        );
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
