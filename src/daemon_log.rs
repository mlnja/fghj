//! In-memory ring buffer of `fghjd`'s own log lines, for the UI's Logs tab.
//! Reset on restart.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

use crate::util::time::now_ms;

const CAPACITY: usize = 2000;

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub seq: u64,
    pub ts_ms: u64,
    pub level: &'static str,
    pub message: String,
}

/// Seq and buffer under one lock: allocating the seq outside it lets two
/// writers append out of order, and `tail(Some(after))` then skips a line
/// forever.
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

/// Also prints, exactly like `println!`.
pub fn info(message: impl Into<String>) {
    let message = message.into();
    println!("{message}");
    push("info", message);
}

pub fn warn(message: impl Into<String>) {
    let message = message.into();
    eprintln!("{message}");
    push("warn", message);
}

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

    // Shared global state: each test only asserts on its own markers.

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
        let baseline = tail(None, 1).last().map(|e| e.seq).unwrap_or(0);
        warn("daemon_log test warn-marker");
        let found = tail(Some(baseline), CAPACITY)
            .into_iter()
            .find(|e| e.message == "daemon_log test warn-marker")
            .expect("just-pushed warn entry should be after the baseline");
        assert_eq!(found.level, "warn");
    }
}
