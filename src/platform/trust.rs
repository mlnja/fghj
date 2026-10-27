use std::path::Path;

use anyhow::Result;

/// The OS trust store fghj's root CA goes into, so browsers accept the
/// certificates it issues.
pub trait TrustStore: Send + Sync {
    /// A no-op when already trusted. Runs on every start, so revoked trust
    /// self-heals.
    fn install(&self, ca_cert_path: &Path) -> Result<()>;

    /// How many certificates were removed.
    fn remove(&self) -> usize;

    /// `None` where unsupported, so `doctor` can tell it from "not trusted".
    fn is_trusted(&self, ca_cert_path: &Path) -> Option<bool>;

    fn store_name(&self) -> &'static str;

    /// Printed by the CLI just before it calls `install` from a terminal.
    fn install_notice(&self) -> &'static str;

    /// The doctor's hint when the CA exists but isn't trusted.
    fn untrusted_hint(&self) -> &'static str;
}
