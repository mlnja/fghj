use std::net::Ipv4Addr;

use anyhow::Result;

/// Extra addresses on the loopback interface, for `web::proxy::PROXY_IP`.
pub trait Loopback: Send + Sync {
    fn add_alias(&self, ip: Ipv4Addr) -> Result<()>;

    /// An address that was never aliased is fine.
    fn remove_alias(&self, ip: Ipv4Addr) -> Result<()>;

    /// `None` where no alias is needed; `Err` when the interface can't be
    /// read.
    fn alias_present(&self, ip: Ipv4Addr) -> Option<Result<bool>>;

    /// For messages, e.g. `lo0`.
    fn interface(&self) -> &'static str;

    /// Whether addresses in `127/8` other than `127.0.0.1` have to be added.
    fn needs_aliases(&self) -> bool;
}
