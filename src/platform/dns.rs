use anyhow::Result;

/// Read back from the OS, so drift from the desired state shows up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsRouting {
    pub location: &'static str,
    pub zones: Vec<(String, u16)>,
}

/// Sends the OS's own lookups for fghj's zones to fghjd's DNS server.
pub trait DnsRouting: Send + Sync {
    /// `zones` is the complete desired set: routing for any zone fghjd set up
    /// earlier and no longer lists is removed. Rerun as wildcard zones come and
    /// go.
    fn sync(&self, port: u16, zones: &[&str]) -> Result<()>;

    /// Otherwise a stopped fghjd leaves the names pointed at a dead port.
    fn clear(&self);

    /// `None` where unsupported, so callers can tell it from "not routed".
    fn status(&self) -> Option<OsRouting>;

    /// What `fghj uninstall` lists as removed, e.g. `/etc/resolver/fghj*`.
    fn footprint(&self) -> Option<&'static str>;
}
