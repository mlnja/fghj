use std::time::{SystemTime, UNIX_EPOCH};

/// `0` if the clock is before the epoch: only used for display and ordering.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
