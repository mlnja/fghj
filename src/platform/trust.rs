use std::path::Path;

use anyhow::Result;

use crate::persistence::WorkspaceOwner;

/// A trust store fghj's root CA goes into, so what reads it accepts the
/// certificates fghj issues: the OS's own, or the browsers' where they keep
/// their own.
pub trait TrustStore: Send + Sync {
    /// Whether the CLI installs it, from the user's terminal, because it asks
    /// for a password a system daemon can't show (macOS). Otherwise fghjd does
    /// it, as root, on every start.
    fn installed_by_user(&self) -> bool;

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

/// A store that belongs to one user, like a browser's own certificate
/// database. fghjd fills it as that user, so their files stay theirs.
pub trait UserTrust: Send + Sync {
    /// A no-op where it's already there.
    fn install(&self, user: &WorkspaceOwner, ca_cert_path: &Path) -> Result<()>;

    /// How many certificates were removed.
    fn remove(&self, user: &WorkspaceOwner) -> usize;

    /// `None` when `user` has no such store, or the platform none at all.
    fn is_trusted(&self, user: &WorkspaceOwner, ca_cert_path: &Path) -> Option<bool>;

    fn store_name(&self) -> &'static str;

    /// The doctor's hint when it's missing for someone.
    fn untrusted_hint(&self) -> &'static str;
}
