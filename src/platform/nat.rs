use anyhow::Result;

use crate::raw_net::RouteSpec;

/// NATs each raw-zone virtual IP to the node's published `127.0.0.1` port.
pub trait RawNat: Send + Sync {
    /// The complete desired state, not a diff.
    fn apply(&self, routes: &[RouteSpec]) -> Result<()>;

    fn clear(&self) -> Result<()>;

    /// The backend's own last-applied record, not recomputed.
    fn status(&self) -> Vec<RouteSpec>;

    /// Those of `routes` the OS isn't actually serving. `None` where raw
    /// routing isn't implemented.
    fn unserved(&self, routes: &[RouteSpec]) -> Option<Result<Vec<RouteSpec>>>;

    /// For the doctor's title, e.g. `pf`.
    fn name(&self) -> &'static str;

    /// Where the rules live, e.g. `pf anchor fghjd`.
    fn location(&self) -> &'static str;

    /// How to look at the loaded rules by hand.
    fn inspect_command(&self) -> &'static str;

    /// What `fghj uninstall` lists as removed, e.g. `the lo0 aliases and the
    /// pf anchor`.
    fn footprint(&self) -> Option<&'static str>;
}
