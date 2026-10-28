use anyhow::Result;

/// What an older fghjd set up for raw-zone ports and this one no longer uses.
/// The raw zone itself is `raw_net::forward` now, the same on every OS.
pub trait LegacyNat: Send + Sync {
    /// Idempotent, and cheap once there's nothing left to remove.
    fn retire(&self) -> Result<()>;
}
