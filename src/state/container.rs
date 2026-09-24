use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::sync_status::SyncStatus;

/// A `*.fghj.internal` name this container answers to, and the `127.0.0.1`
/// port Docker published its backing container-side port on when the route
/// was derived — the SNI -> backend lookup `web::proxy::serve_https` dispatches
/// real per-service HTTPS routing through (see `state::query::resolve_route`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRoute {
    pub domain: String,
    /// The host port as of when this route was derived. Read through
    /// `state::query::live_host_port`, which prefers the freshly-observed
    /// binding for `container_port` and only falls back to this — so a
    /// container Docker republished on a different ephemeral port routes
    /// correctly without anything having to rewrite stored routes.
    pub host_port: u16,
    /// When set, `domain` is a suffix from `Node.wildcard_hosts` rather
    /// than an exact name — `state::query::resolve_route` matches it
    /// against the suffix itself *and* any subdomain of it.
    #[serde(default)]
    pub wildcard: bool,
    /// The container-side port (a key into `ContainerObserved::ports`) this
    /// route was derived from — what lets `host_port` be re-joined against
    /// a fresh observation without needing the original `Node` config back.
    /// `#[serde(default)]` so a route persisted before this field existed
    /// deserializes as `""`, which simply never matches and leaves the
    /// stored `host_port` in play until the container is next started.
    #[serde(default)]
    pub container_port: String,
    /// Whether `domain` is eligible for a cert from fghj's local CA — the
    /// same `dns::cert_eligible` rule `web::ca::DynamicCertResolver::resolve_for`
    /// applies at TLS-handshake time, computed once at route-derivation
    /// time so the UI doesn't need its own copy of the rule. Always `true`
    /// for a node's convention-derived `*.fghj.internal` domain; only
    /// variable for an author-declared `additional_hosts`/`wildcard_hosts`
    /// alias, since only those can name a real, non-reserved TLD (e.g. a
    /// third-party OAuth callback host) fghj's CA will never certify — the
    /// UI links such a route as `http://` rather than an `https://` link
    /// that would always fail with a TLS error.
    #[serde(default = "cert_eligible_by_default")]
    pub https: bool,
}

/// A route persisted before `https` existed predates `additional_hosts`
/// too, so it can only be a convention-derived `*.fghj.internal` name —
/// always cert-eligible.
fn cert_eligible_by_default() -> bool {
    true
}

/// Which start/stop/delete action is currently in flight for a container —
/// the transient half of a node's lifecycle, layered on top of
/// `ContainerObserved::status` (which only ever reflects Docker's own
/// settled state: running/exited/removed).
///
/// The single record of that fact. The reducer sets it when it accepts a
/// request, checks it for in-flight dedup
/// (`container.pending_action.is_some()` → `ActionRejected::AlreadyInFlight`),
/// and clears it on `Action::ContainerActionSettled` once the effect doing
/// the real Docker call finishes — see `reducer::run` and
/// `reducer::observation`. `RunRegistry` used to keep a second, separate
/// `pending` map guarding the same thing, which meant two gates that could
/// disagree about whether a node was busy.
///
/// Also the one truth the UI needs to answer "what is this node doing right
/// now", so it never has to infer that from its own click history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingAction {
    Starting,
    Stopping,
    Removing,
}

impl std::fmt::Display for PendingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingAction::Starting => write!(f, "starting"),
            PendingAction::Stopping => write!(f, "stopping"),
            PendingAction::Removing => write!(f, "removing"),
        }
    }
}

/// Everything about a container fghj *wants* to be true — the half of
/// `ContainerInfo` a reducer ever writes to (aside from `pending_action`),
/// and the half a convergence effect reads to decide whether/how to act on
/// Docker. Split out from `ContainerObserved` (what Docker actually
/// reports right now) because the two change on entirely different
/// triggers: `desired` only changes in response to a dispatched `Action`
/// (`RunPlanned`, `RunNodeStartRequested`, ...); `observed` only changes in
/// response to the Docker-polling effect's own inspection. Everything here
/// is computed from a `NodeSpec`, never read back off a live container —
/// that is exactly what makes `observed != desired` a meaningful reading
/// rather than a tautology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ContainerDesired {
    /// Whether fghj has been asked for this container to be up. Set from
    /// the *intent* of whatever call produced this container (a start says
    /// `true` even if it crashed a millisecond later), never from
    /// `ContainerObserved::status` — deriving it from the outcome would
    /// make the two agree by construction and hide every drift the UI's
    /// "desired ≠ actual" indicator exists to show.
    pub running: bool,
    pub container_name: String,
    pub domain: String,
    pub raw_domain: String,
    pub routes: Vec<PortRoute>,
    /// The subset of `Node.additional_hosts` that actually got a route
    /// (i.e. the node has a `primary` port) — kept separate from `routes`
    /// (which also carries the node's own derived-domain and named-port
    /// routes) because `effects::hosts::HostsEffect` needs exactly this
    /// list, and only this list, to sync `/etc/hosts`: a `*.fghj.internal`
    /// route is already served by fghjd's own DNS, and would be actively
    /// wrong to also pin as a static `/etc/hosts` entry.
    pub additional_hosts: Vec<String>,
    /// The container-side port (a key into `ContainerObserved::ports`) that
    /// `status`/`published_port` are inspected against — `node.ports`'
    /// `primary` entry, or an arbitrary declared port if none is marked
    /// `primary` (see the selection logic in `start_node`). `None` for a
    /// node with no declared ports at all, matching
    /// `docker::inspect_status`'s own "inspect status only" mode. Kept
    /// around rather than re-derived so `RunRegistry::inspect_containers` can
    /// re-inspect the same port `start_node` picked without needing the
    /// original `Node` config back.
    pub status_port: Option<String>,
    /// Hex-encoded hash of everything about this node's resolved config
    /// that actually affects how the container runs (image, command, env,
    /// ports, volumes, ...) at the moment it was last actually started
    /// through fghj — see `runs::spec::spec_hash`. Compared against a
    /// freshly recomputed hash of the *current* `.fghj.yaml` by
    /// `RunRegistry::config_drift` to detect drift; never used to decide
    /// anything on its own.
    pub config_hash: String,
}

/// Everything about a container Docker itself last reported — the
/// convergence-target-agnostic half of `ContainerInfo`; see
/// `ContainerDesired`'s doc for why the split exists. Only ever written
/// from a real inspection (`RunRegistry::inspect_containers`,
/// `Action::ContainerObserved`), never from what fghj asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ContainerObserved {
    pub status: String,
    pub published_port: Option<u16>,
    /// The container's address on its Docker network. `None` until
    /// something actually inspects it — `RunRegistry` has never needed it,
    /// so in practice only `Action::ContainerObserved` can set it.
    pub ip: Option<String>,
    /// The host-published port for every one of this node's declared
    /// ports, not just the routed (`primary`/`name`d) ones — lets the UI
    /// offer a direct `127.0.0.1:<port>` connection string for a plain TCP
    /// backing dependency (postgres, mysql) with no HTTP surface to route
    /// at all. `None` for a port Docker hasn't actually published
    /// (container not running, or no live binding yet).
    pub ports: BTreeMap<String, Option<u16>>,
    pub sync: SyncStatus,
}

/// One container's full state: what fghj wants (`desired`), what Docker
/// last reported (`observed`), and what's currently in flight
/// (`pending_action`). The single representation — the reducer, the
/// `RunRegistry` that drives Docker, SQLite, and the `/runs` JSON the UI
/// reads all use this one type, so there is nothing to translate between
/// and nothing that can be lost in translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerInfo {
    pub node_id: String,
    pub desired: ContainerDesired,
    pub observed: ContainerObserved,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_action: Option<PendingAction>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired() -> ContainerDesired {
        ContainerDesired {
            running: true,
            container_name: "fghj-web-1".into(),
            domain: "web.fghj.internal".into(),
            raw_domain: "web.fghj.raw.internal".into(),
            routes: vec![],
            additional_hosts: vec![],
            status_port: Some("80".into()),
            config_hash: "abc123".into(),
        }
    }

    #[test]
    fn container_info_carries_a_fresh_container_with_no_pending_action() {
        let info = ContainerInfo {
            node_id: "web".into(),
            desired: desired(),
            observed: ContainerObserved::default(),
            pending_action: None,
        };
        assert!(info.pending_action.is_none());
        assert_eq!(info.observed.sync, SyncStatus::Unknown);
    }

    #[test]
    fn pending_action_display_matches_serde_rename() {
        assert_eq!(PendingAction::Starting.to_string(), "starting");
        assert_eq!(PendingAction::Stopping.to_string(), "stopping");
        assert_eq!(PendingAction::Removing.to_string(), "removing");
    }

    #[test]
    fn pending_action_omitted_from_json_when_none() {
        let info = ContainerInfo {
            node_id: "web".into(),
            desired: desired(),
            observed: ContainerObserved::default(),
            pending_action: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("pending_action"));
    }

    #[test]
    fn pending_action_present_in_json_when_set() {
        let info = ContainerInfo {
            node_id: "web".into(),
            desired: desired(),
            observed: ContainerObserved::default(),
            pending_action: Some(PendingAction::Starting),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"pending_action\":\"starting\""));
    }
}
