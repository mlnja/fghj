use std::time::{SystemTime, UNIX_EPOCH};

/// The current unix time in milliseconds, or `0` if the system clock is set
/// before the epoch. Every caller uses this for display or for ordering
/// against other values from this same function, so a nonsense clock is
/// worth degrading on rather than panicking over.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
