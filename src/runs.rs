use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dns;
use crate::docker;
use crate::persistence::{self, LogLine, WorkspaceDb};
use crate::resolver::{Edge, Graph, Healthcheck, Node, VolumeMount};

pub const DEFAULT_RUN_ID: &str = "default";

fn sanitize_label(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::new();
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// The concrete run id `start` derives from a `RunSpec::run_id` — sanitized,
/// falling back to `DEFAULT_RUN_ID` when absent or empty after sanitizing.
/// Exposed so `daemon::post_runs` can compute the same id up front to
/// dispatch `Action::RunPlanned` under, before `start`/`ensure_running`
/// (now called from inside `effects::docker::converge::perform_create`,
/// not synchronously from the HTTP handler) ever runs.
pub fn resolve_run_id(run_id: Option<&str>) -> String {
    run_id
        .map(sanitize_label)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RUN_ID.to_string())
}

#[derive(Debug, Deserialize, Clone)]
pub struct RunSpec {
    #[serde(default)]
    pub run_id: Option<String>,
    /// Scopes the run to only the nodes reachable from this flow (see
    /// `Node::flows`) instead of the whole graph — e.g. starting just the
    /// checkout flow's services instead of every service fghj knows about.
    #[serde(default)]
    pub flow: Option<String>,
}

/// Which of the two zones a derived domain belongs to — see `dns.rs`'s
/// module doc for the full split. `Http` is the proxy/SNI-dispatched,
/// same-address-in-or-out zone (`fghj.internal`, unchanged from before this
/// split existed); `Raw` is the new in-network-only zone
/// (`fghj.raw.internal`) that resolves straight to a container's own IP via
/// Docker's native per-network DNS, for callers that need a real port
/// number raw TCP can't safely multiplex behind one shared address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainZone {
    Http,
    Raw,
}

impl DomainZone {
    fn suffix(self) -> &'static str {
        match self {
            DomainZone::Http => "fghj.internal",
            DomainZone::Raw => "fghj.raw.internal",
        }
    }
}

/// Derives a node's canonical domain in the given zone for a given run — the
/// single definition `start_node` uses when actually launching a container,
/// also called from `resolver::resolve_universe` (always with
/// `DEFAULT_RUN_ID` and `DomainZone::Http`) so `Node.domain` can carry a
/// node's default-run address before any container for it has ever been
/// started. Two nodes can never collide on the result: `node_id` is already
/// the unique, leaf-first id (see `resolver::visit_local_service`/
/// `visit_dependency`), and `run_id` is folded in for every run except the
/// default one (see `start_node`'s own comment for why).
pub fn derive_domain(
    node_id: &str,
    domain_scope: &str,
    workspace_name: &str,
    run_id: &str,
    zone: DomainZone,
) -> String {
    let workspace = sanitize_label(workspace_name);
    let suffix = zone.suffix();
    if domain_scope == "stable" || run_id == DEFAULT_RUN_ID {
        format!("{node_id}.{workspace}.{suffix}")
    } else {
        format!("{node_id}.{run_id}.{workspace}.{suffix}")
    }
}

/// Expands `${FGHJ_SERVICE_FQDN}`/`${FGHJ_SERVICE_FQDN:path}` (this node's
/// own, or a sibling's, `fghj.raw.internal` domain — see `sibling_domain`
/// for what `path` can look like) and `${FGHJ_SERVICE_FQDN_HTTP}`/
/// `${FGHJ_SERVICE_FQDN_HTTP:path}` (the `fghj.internal` — proxied — domain
/// instead) in a single `environment`/`env_file` value, so a CUE author can
/// reference a derived address without hand-computing `derive_domain`'s
/// formula into a literal string (the convention every hardcoded
/// `*_HOST`/`*_URL` value in `aikido-core`/`aikifactory`'s `.fghj.yaml`
/// followed before this existed). The bare `FQDN` form resolves to the raw
/// zone — direct container access — because that's what every real caller
/// of this macro today actually needs (a database connection string, a raw
/// S3 endpoint); `_HTTP` is the rare opt-in for a service's own *proxied*
/// identity (e.g. a presigned URL meant to be handed to something outside
/// the network). The longer `_HTTP` token is checked first so it's never
/// mistaken for the shorter one plus a literal `_HTTP` suffix. A manual scan
/// rather than the `regex` crate (not otherwise a dependency) — the grammar
/// is just those forms, simple enough that a scanner is less code than
/// pulling in a new crate. An unresolvable `:path` (no such sibling) or a
/// token missing its closing `}` is left untouched in the output rather
/// than erroring — a typo here shouldn't fail an entire run when the
/// literal fallback is at least diagnosable in logs, the same tolerance
/// `parse_env_file` extends to a malformed line.
fn expand_service_fqdn_templates(
    value: &str,
    node: &Node,
    own_raw_domain: &str,
    own_http_domain: &str,
    graph: &Graph,
    run_id: &str,
) -> String {
    // `TOKEN_RAW` is a literal prefix of `TOKEN_HTTP`, so `rest.find`ing it
    // always lands on the truly leftmost occurrence of either token — a
    // standalone `_HTTP` search alone would miss the case where the
    // leftmost token is actually the plain (raw) form, and searching both
    // separately would need extra tie-breaking since they can share a start
    // index. Whichever one `find` lands on, a cheap `starts_with` check at
    // that position tells the two apart.
    const TOKEN_HTTP: &str = "${FGHJ_SERVICE_FQDN_HTTP";
    const TOKEN_RAW: &str = "${FGHJ_SERVICE_FQDN";
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let Some(start) = rest.find(TOKEN_RAW) else {
            break;
        };
        let (token_len, zone, own_domain) = if rest[start..].starts_with(TOKEN_HTTP) {
            (TOKEN_HTTP.len(), DomainZone::Http, own_http_domain)
        } else {
            (TOKEN_RAW.len(), DomainZone::Raw, own_raw_domain)
        };
        out.push_str(&rest[..start]);
        let Some(end_rel) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let end = start + end_rel;
        let inner = &rest[start + token_len..end]; // "" or ":name"
        let resolved = match inner.strip_prefix(':') {
            None => Some(own_domain.to_string()),
            Some(name) => sibling_domain(node, name, graph, run_id, zone),
        };
        match resolved {
            Some(domain) => out.push_str(&domain),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Finds the domain `${FGHJ_SERVICE_FQDN:path}` means from `node`'s own
/// `environment`, in two ways (first match wins):
///
/// - A backing dependency matching `path` that shares an "owns" owner with
///   `node` — the owner is whoever's "owns" edge points at `node` (a
///   backing dependency looking for a sibling backing dependency), or
///   `node.id` itself if nothing owns it (a service looking up one of its
///   own directly-declared backing dependencies).
/// - A service matching `path` that `node` directly depends on via a
///   `kind: service` dependency (same-repo or cross-repo — both produce a
///   "depends-on" edge from `node.id`, see `resolver::visit_dependency`).
///   This is the only way to reach a sibling *service*: unlike backing
///   dependencies, services aren't owned, so there's no shared-owner case
///   to fall back on — only what `node` itself declares a dependency on.
///
/// `path` is one bare name (`mysql`) in the common case — matched against
/// just the candidate's own leaf name — or `::`-separated segments
/// (`aikifactory::aikifactory::minio`) for the rare case where that's
/// ambiguous. A node's `id` is already the leaf-first chain the domain
/// itself is built from (`{name}.{owner-id}`, see `resolver::visit_dependency`
/// /`visit_local_services`) — root-first is just easier to read/write, so
/// `path`'s segments are reversed and dot-joined into that same shape
/// before matching, e.g. `aikifactory::aikifactory::minio` becomes
/// `minio.aikifactory.aikifactory`, an exact prefix of the real id
/// `minio.aikifactory.aikifactory` (before the workspace/`fghj.internal`
/// suffix `derive_domain` appends). Fewer segments than the full id just
/// means "match any id with this as a trailing-toward-the-root prefix" —
/// as many as it takes to stop being ambiguous, no more.
fn sibling_domain(
    node: &Node,
    path: &str,
    graph: &Graph,
    run_id: &str,
    zone: DomainZone,
) -> Option<String> {
    let mut segments: Vec<&str> = path.split("::").collect();
    segments.reverse();
    let id_prefix = segments.join(".");
    let matches = |candidate: &Node| {
        candidate.id == id_prefix || candidate.id.starts_with(&format!("{id_prefix}."))
    };

    let owner_id = graph
        .edges
        .iter()
        .find(|e| e.kind == "owns" && e.to == node.id)
        .map(|e| e.from.as_str())
        .unwrap_or(node.id.as_str());
    let sibling = graph
        .edges
        .iter()
        .filter(|e| e.kind == "owns" && e.from == owner_id)
        .find_map(|e| graph.nodes.iter().find(|n| n.id == e.to && matches(n)))
        .or_else(|| {
            graph
                .edges
                .iter()
                .filter(|e| e.kind == "depends-on" && e.from == node.id)
                .find_map(|e| graph.nodes.iter().find(|n| n.id == e.to && matches(n)))
        })?;
    Some(derive_domain(
        &sibling.id,
        &sibling.domain_scope,
        &graph.workspace_name,
        run_id,
        zone,
    ))
}

/// One entry in the route table `write_route_table` persists for a run's
/// sidecar proxy to poll (see `src/bin/fghj-sidecar.rs`'s own
/// field-name-matching `RouteFileEntry`, kept as a separate type so that
/// binary doesn't need to depend on this module at all). Needs no raw
/// IP/container-name derivation: `connect_host` is the container's
/// `fghj.raw.internal` domain, a real Docker network alias (see
/// `resolve_node_spec`'s `aliases`) that Docker's own embedded per-network
/// DNS resolves for any container on the network, including the sidecar
/// itself — the `fghj.internal` domain (`lookup`) is deliberately *not* a
/// Docker alias on the node's own container anymore, since the sidecar
/// itself is what needs to own that name's resolution.
#[derive(Debug, PartialEq, Serialize)]
struct RouteFileEntry {
    lookup: String,
    wildcard: bool,
    connect_host: String,
    connect_port: u16,
}

/// The pure part of `write_route_table` — every routable domain across
/// `containers`, connecting via each container's own `raw_domain` (see
/// `RouteFileEntry`'s doc comment for why). Split out from the actual file
/// write so it can be unit-tested without touching `/var/lib/fghjd`, which
/// is root-owned in production.
fn route_file_entries(containers: &[ContainerInfo]) -> Vec<RouteFileEntry> {
    containers
        .iter()
        .flat_map(|c| {
            c.routes.iter().filter_map(move |r| {
                let connect_port = r.container_port.split('/').next()?.parse().ok()?;
                Some(RouteFileEntry {
                    lookup: r.domain.clone(),
                    wildcard: r.wildcard,
                    connect_host: c.raw_domain.clone(),
                    connect_port,
                })
            })
        })
        .collect()
}

fn sidecar_routes_dir(network: &str) -> PathBuf {
    persistence::fghjd_root().join("runs").join(network)
}

fn sidecar_routes_path(network: &str) -> PathBuf {
    sidecar_routes_dir(network).join("routes.json")
}

fn sidecar_ca_dir() -> PathBuf {
    persistence::fghjd_root().join("sidecar-ca")
}

/// A world-readable copy of the CA cert+key, refreshed on every sidecar
/// (re)creation, kept separate from the real `daemon::ca_dir()` — `fghjd`
/// runs as root and the real `ca-key.pem` is deliberately `0600`
/// root-owned, but Docker Desktop/OrbStack's bind-mount sharing on macOS is
/// brokered by a process running as the logged-in user, not root: even a
/// container claiming to run as `root` can't read a `0600` root-owned file
/// through that bridge, since the permission check happens on the host side
/// against the real user, before the request ever reaches the container's
/// own UID namespace. Mounting the CA into a container at all is already
/// the accepted tradeoff for this feature (see `fghj-sidecar.rs`'s own
/// doc comment); this only has to be readable by whoever is already running
/// `sudo fghjd` on this machine, which is a strictly smaller exposure.
fn refresh_sidecar_ca_copy() -> Result<PathBuf> {
    let dir = sidecar_ca_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {dir:?}"))?;

    let src_dir = crate::daemon::ca_dir();
    for (src, dest_name) in [
        (crate::ca::ca_cert_path(&src_dir), "ca-cert.pem"),
        (crate::ca::ca_key_path(&src_dir), "ca-key.pem"),
    ] {
        let bytes = std::fs::read(&src).with_context(|| format!("failed to read {src:?}"))?;
        let dest = dir.join(dest_name);
        std::fs::write(&dest, bytes).with_context(|| format!("failed to write {dest:?}"))?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644))
            .with_context(|| format!("failed to set permissions on {dest:?}"))?;
    }

    Ok(dir)
}

/// Regenerates the route table this run's sidecar proxy polls from, from
/// `state.containers` alone — every routable domain across every container
/// this run knows about, whether or not that container's own status is
/// currently `running` (matching a stopped-then-restarted route staying
/// valid the moment the container comes back). Best-effort: a write failure
/// here shouldn't fail the start/stop call it's riding along on, since the
/// next lifecycle call for this run retries it anyway.
fn write_route_table(state: &RunState) -> Result<()> {
    let dir = sidecar_routes_dir(&state.network);
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {dir:?}"))?;

    let entries = route_file_entries(&state.containers);

    let path = sidecar_routes_path(&state.network);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&entries)?)
        .with_context(|| format!("failed to write {tmp:?}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("failed to rename into {path:?}"))?;
    Ok(())
}

/// Derives the real Docker volume name for a `VolumeMount::Named` entry —
/// reuses `derive_domain`'s exact run/stable folding logic (a named
/// volume's `scope` is the same knob as `domain_scope`), keyed by the
/// volume's own declared `name` instead of a node id. Two nodes anywhere in
/// the graph that declare the same `name` + `scope` therefore land on the
/// same derived value here and transparently share one Docker volume.
fn derive_volume_name(name: &str, scope: &str, workspace_name: &str, run_id: &str) -> String {
    format!(
        "fghj-vol-{}",
        sanitize_label(&derive_domain(
            name,
            scope,
            workspace_name,
            run_id,
            DomainZone::Http,
        ))
    )
}

/// Orders `node_ids` so that every node's dependencies — an edge's `to` (see
/// `resolver::Edge`'s own doc comment: `to` is always the dependency, `from`
/// always the dependent, for every edge kind) — are started before it. Used
/// by both `start` and `ensure_running` so containers come up in dependency
/// order instead of whatever order `graph.nodes` happens to iterate in.
/// Edges pointing outside `node_ids` (e.g. a flow-filtered run that excludes
/// a node's dependency) are ignored — nothing to order against.
///
/// A cycle can't make progress by definition; rather than fail the whole
/// run over a cyclic `.fghj.yaml` (resolution here is independent of `fghj
/// validate` — see the module doc on that split), whatever's left over is
/// appended in stable sorted order so a run still starts *something*.
pub fn topological_start_order(node_ids: &[String], edges: &[Edge]) -> Vec<String> {
    let ids: HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::new();
    for id in node_ids {
        deps.entry(id.as_str()).or_default();
    }
    for edge in edges {
        if ids.contains(edge.from.as_str()) && ids.contains(edge.to.as_str()) {
            deps.entry(edge.from.as_str())
                .or_default()
                .insert(edge.to.as_str());
        }
    }

    let mut ordered: Vec<String> = Vec::new();
    let mut placed: HashSet<&str> = HashSet::new();
    // Sorted, not hashmap-iteration-order, so ties (and the cycle fallback
    // below) are stable across calls.
    let mut remaining: Vec<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    remaining.sort_unstable();

    while !remaining.is_empty() {
        let mut next_remaining = Vec::new();
        let mut progressed = false;
        for id in &remaining {
            if deps[id].iter().all(|d| placed.contains(d)) {
                ordered.push(id.to_string());
                placed.insert(id);
                progressed = true;
            } else {
                next_remaining.push(*id);
            }
        }
        remaining = next_remaining;
        if !progressed {
            ordered.extend(remaining.into_iter().map(str::to_string));
            break;
        }
    }
    ordered
}

/// Polls a container's declared healthcheck (if any) until it reports
/// "healthy", for up to two minutes — long enough for a real database's own
/// startup healthcheck, short enough that a genuinely broken one doesn't
/// hang a run forever. Returns as soon as there's nothing more to wait for:
/// no declared healthcheck, a terminal "unhealthy" report (best-effort —
/// fghj proceeds rather than blocking the run indefinitely), or the
/// container having vanished. Callers only call this at all when
/// `node.healthcheck.is_some()`, but it's written to be a safe no-op
/// otherwise too.
async fn wait_for_healthy(docker: &bollard::Docker, container_name: &str) {
    for _ in 0..60 {
        match docker::inspect_health(docker, container_name).await {
            Ok(Some(status)) if status == "healthy" || status == "unhealthy" => return,
            Ok(None) => return,
            _ => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Parses a `.env`-style file's contents into "KEY=value" pairs — blank
/// lines and `#`-comments are skipped, and matching surrounding quotes on
/// the value are stripped (the common `.env` convention). No multi-line
/// values or `export` prefixes: real `.env` files in the wild are simple
/// enough that this covers the practical cases, same scope Compose's own
/// `env_file` support covers.
fn parse_env_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let mut value = value.trim();
            if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                value = &value[1..value.len() - 1];
            }
            Some(format!("{key}={value}"))
        })
        .collect()
}

/// A `*.fghj.internal` name this container answers to, and the `127.0.0.1`
/// port Docker actually published its backing container-side port on — the
/// SNI -> backend lookup `proxy::serve_https` dispatches real per-service
/// HTTPS routing through (see `WorkspaceRegistry::resolve_route`).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PortRoute {
    pub domain: String,
    pub host_port: u16,
    /// When set, `domain` is a suffix from `Node.wildcard_hosts` rather than
    /// an exact name — `WorkspaceRegistry::resolve_route` matches it against
    /// the suffix itself *and* any subdomain of it, not just an exact
    /// string. `#[serde(default)]` so rows persisted before this field
    /// existed just deserialize as `false` (an ordinary exact route).
    #[serde(default)]
    pub wildcard: bool,
    /// The container-side port (a key into `ContainerInfo.ports`) this route
    /// was derived from — lets `RunRegistry::refresh` re-inspect just that
    /// binding and correct `host_port` if Docker republishes the container
    /// on a different ephemeral host port (e.g. a restart-policy-triggered
    /// restart, or `dockerd` itself restarting), without needing the
    /// original `Node` config back. `#[serde(default)]` so a route persisted
    /// before this field existed just deserializes as `""` — `refresh` skips
    /// those until the owning container is next started through `fghj`,
    /// which recomputes routes from scratch anyway.
    #[serde(default)]
    pub container_port: String,
    /// Whether `domain` is eligible for a cert from fghj's local CA — the
    /// exact same `dns::cert_eligible` rule `ca::DynamicCertResolver::resolve_for`
    /// applies at TLS-handshake time, computed once here (every route pushed
    /// below is routed by construction) so the UI doesn't need its own copy
    /// of the rule. Always `true` for the node's own convention-derived
    /// `*.fghj.internal` domain/named ports; only actually variable for an
    /// author-declared `additional_hosts`/`wildcard_hosts` alias, since only
    /// those can name a real, non-reserved TLD (e.g. a third-party OAuth
    /// callback host) that fghj's CA will never certify — the UI links such
    /// a route as `http://`, not a `https://` link that would always fail
    /// with a TLS error. `#[serde(default = "default_https_eligible")]`
    /// assumes the common case for a route persisted before this field
    /// existed, until the owning container is next started through `fghj`.
    #[serde(default = "default_https_eligible")]
    pub https: bool,
}

fn default_https_eligible() -> bool {
    true
}

#[derive(Debug, Serialize, Clone)]
pub struct ContainerInfo {
    pub node_id: String,
    pub container_name: String,
    pub status: String,
    pub published_port: Option<u16>,
    pub domain: String,
    /// This node's `fghj.raw.internal` domain — the real Docker network
    /// alias for the node's own container (see `resolve_node_spec`'s
    /// `aliases`), used by `write_route_table` as the sidecar's
    /// `connect_host` for relaying `domain`'s HTTP(S) traffic to the real
    /// backend. `#[serde(default)]` so a row persisted before this field
    /// existed just deserializes empty, until the container is next started
    /// through fghj.
    #[serde(default)]
    pub raw_domain: String,
    pub routes: Vec<PortRoute>,
    /// The subset of `Node.additional_hosts` that actually got a route (i.e.
    /// the node has a `primary` port) — kept separate from `routes` (which
    /// also carries the node's own derived-domain and named-port routes)
    /// because `effects::hosts::HostsEffect` needs exactly this list, and
    /// only this list, to sync `/etc/hosts`: a
    /// `*.fghj.internal` route is already served by fghjd's own DNS, and
    /// would be actively wrong to also pin as a static `/etc/hosts` entry.
    #[serde(default)]
    pub additional_hosts: Vec<String>,
    /// The host-published port for every one of this node's declared ports,
    /// not just the routed (`primary`/`name`d) ones — lets the UI offer a
    /// direct `127.0.0.1:<port>` connection string for a plain TCP backing
    /// dependency (postgres, mysql) that has no HTTP surface to route at
    /// all, alongside the `*.fghj.internal` links `routes` already covers.
    /// `None` for a port Docker hasn't actually published (container not
    /// running, or the port entry has no live binding yet).
    #[serde(default)]
    pub ports: BTreeMap<String, Option<u16>>,
    /// The container-side port (a key into `ports`) that `status`/
    /// `published_port` are inspected against — `node.ports`' `primary`
    /// entry, or an arbitrary declared port if none is marked `primary` (see
    /// the selection logic in `start_node`). `None` for a node with no
    /// declared ports at all, matching `docker::inspect_status`'s own
    /// "inspect status only" mode. Kept around (rather than re-derived) so
    /// `RunRegistry::refresh` can re-inspect the same port `start_node`
    /// picked without needing the original `Node` config. `#[serde(default)]`
    /// so a row persisted before this field existed just deserializes as
    /// `None` — `refresh` still updates `status`, just not `published_port`,
    /// for that container until it's next started through `fghj`.
    #[serde(default)]
    pub status_port: Option<String>,
    /// Hex-encoded hash of everything about this node's resolved config that
    /// actually affects how the container runs (image, command, env, ports,
    /// volumes, ...) at the moment it was last actually started through
    /// fghj — see `spec_hash`. Compared against a freshly recomputed hash of
    /// the *current* `.fghj.yaml` by `RunRegistry::refresh_sync_status` to
    /// detect drift; never used to decide anything on its own. Empty for a
    /// container persisted before this field existed, or by a code path that
    /// doesn't have a `NodeSpec` to hash (shouldn't happen for anything
    /// `start_node` itself produced).
    #[serde(default)]
    pub config_hash: String,
    /// Whether `config_hash` still matches what `.fghj.yaml` would produce
    /// right now — `None` until the first background sync check runs (or
    /// for a branch-overridden service, which drift-checking can't safely
    /// recompute without doing a real git checkout — see `resolve_node_spec`).
    /// Purely informational: the "Desired state" vs "Actual state" indicator
    /// in the Drawer, never anything `fghj` acts on by itself — recreating a
    /// drifted container is always the user's own explicit Start/Restart.
    #[serde(default)]
    pub synced: Option<bool>,
    /// Which start/stop/delete action (if any) is currently in flight for
    /// this node, per `RunRegistry`'s `pending` map — never persisted (not a
    /// DB column; always `None` coming out of `persistence::sqlite` or a freshly-built
    /// `ContainerInfo`) and never read by anything but `RunRegistry::list`,
    /// which fills it in live from `pending` right before handing state back
    /// to the API. This is the one truth the UI needs to know "what is this
    /// node doing right now" — it should stop inferring it from its own
    /// click history.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pending_action: Option<PendingAction>,
}

/// A start/stop/delete call currently claimed in `RunRegistry::pending` for
/// some node — the transient half of a node's lifecycle, layered on top of
/// `ContainerInfo::status` (which only ever reflects Docker's own settled
/// state: running/exited/removed). Surfaced to the API via
/// `ContainerInfo::pending_action` so the frontend can render "starting…" /
/// disable buttons off real backend state instead of guessing from its own
/// in-flight requests.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Serialize, Clone)]
pub struct RunState {
    pub run_id: String,
    pub network: String,
    pub containers: Vec<ContainerInfo>,
    /// The deterministic name of this run's in-network TLS proxy sidecar
    /// (see `RunRegistry::ensure_sidecar`) — one per run, never shared
    /// across workspaces. Empty for a `RunState` persisted before this field
    /// existed, until that run is next started/topped up.
    #[serde(default)]
    pub sidecar_container_name: String,
    /// The sidecar's own address on `network` — `None` until
    /// `docker::inspect_network_ip` has actually resolved it (or for a
    /// pre-sidecar persisted `RunState`). Cached here rather than
    /// re-inspected on every node start so `start_node` doesn't need a
    /// Docker round-trip just to set every node's `--dns`.
    #[serde(default)]
    pub sidecar_ip: Option<String>,
}

/// The pure, side-effect-free result of `RunRegistry::resolve_node_spec` —
/// everything about a node's config that has to be actually computed (as
/// opposed to read straight off `Node`) before it can be either run for real
/// (`start_node`) or hashed to check for drift (`spec_hash`,
/// `refresh_sync_status`).
struct NodeSpec {
    container_name: String,
    domain: String,
    raw_domain: String,
    aliases: Vec<String>,
    image: String,
    port_list: Vec<(String, Option<u16>)>,
    binds: Vec<String>,
    env: Vec<String>,
}

/// Hashes everything about `node` + `spec` that actually affects how the
/// container runs, for `ContainerInfo::config_hash` /
/// `RunRegistry::refresh_sync_status` to compare against. Deliberately
/// excludes anything that's either pure identity (`container_name`, `domain`,
/// `aliases` all derive one-to-one from `node.id` + run id — never drift
/// independently of the rest of the spec) or genuinely ephemeral (an
/// unpinned port's actual host-side binding is chosen fresh by Docker on
/// every start; hashing it would flag every single restart as "drifted").
/// Uses `Sha256` rather than `std::hash::DefaultHasher`, which is explicitly
/// documented as unstable across Rust versions — that would misreport a
/// clean upgrade of the `fghj` binary itself as config drift.
fn spec_hash(node: &Node, spec: &NodeSpec) -> String {
    #[derive(Serialize)]
    struct DesiredSpec<'a> {
        image: &'a str,
        command: &'a [String],
        env: &'a [String],
        ports: &'a [(String, Option<u16>)],
        binds: &'a [String],
        restart: &'a str,
        user: Option<&'a str>,
        working_dir: Option<&'a str>,
        labels: &'a BTreeMap<String, String>,
        cap_add: &'a [String],
        cap_drop: &'a [String],
        privileged: bool,
        extra_hosts: &'a [String],
        healthcheck: Option<&'a Healthcheck>,
        platform: Option<&'a str>,
    }

    let desired = DesiredSpec {
        image: &spec.image,
        command: &node.command,
        env: &spec.env,
        ports: &spec.port_list,
        binds: &spec.binds,
        restart: &node.restart,
        user: node.user.as_deref(),
        working_dir: node.working_dir.as_deref(),
        labels: &node.labels,
        cap_add: &node.cap_add,
        cap_drop: &node.cap_drop,
        privileged: node.privileged,
        extra_hosts: &node.extra_hosts,
        healthcheck: node.healthcheck.as_ref(),
        platform: node.platform.as_deref(),
    };
    let bytes = serde_json::to_vec(&desired).expect("DesiredSpec always serializes");
    let digest = Sha256::digest(&bytes);
    format!("{digest:x}")
}

pub struct RunRegistry {
    workspace: std::path::PathBuf,
    db: Arc<WorkspaceDb>,
    docker: Arc<bollard::Docker>,
    runs: Mutex<BTreeMap<String, RunState>>,
    /// Serializes `restart_container`/`stop_container`/`remove_container`
    /// across the *whole workspace*, not per node: two concurrent lifecycle
    /// calls for the same node both `docker::stop_and_remove` then recreate
    /// the same container name, which Docker itself will reject for
    /// whichever loses the race, and interleaving two *different* nodes'
    /// Docker calls arbitrarily isn't obviously safe either. An `Arc`'d
    /// workspace-wide `tokio::sync::Mutex` (must survive an `.await`, unlike
    /// the plain `std::sync::Mutex` above that only ever guards a quick
    /// snapshot/write-back) makes concurrent calls run one at a time instead
    /// of racing. On its own this only reorders work, though — see `pending`
    /// below for what actually stops a duplicate from running at all.
    action_lock: tokio::sync::Mutex<()>,
    /// Node ids with a start/stop/delete currently in flight, and which one.
    /// `action_lock` alone only *serializes* concurrent calls — a second
    /// "start" for a node already starting would just wait its turn behind
    /// the lock, then go on to redundantly stop-and-recreate the container
    /// the first call just started. Checking (and inserting into) this map
    /// *before* ever waiting on `action_lock` lets `begin_action` reject
    /// that second call immediately instead of queuing it to run anyway.
    /// Also the source of truth `list()` reads to fill in
    /// `ContainerInfo::pending_action` for the API/UI.
    pending: Mutex<HashMap<String, PendingAction>>,
    /// Background per-`(run_id, node_id)` tasks streaming that node's
    /// *current* container's logs into `db` as they're produced — see
    /// `spawn_log_capture`. Keyed so that starting a node again aborts its
    /// previous capture task before replacing it, guaranteeing at most one
    /// task (and one open generation) writing for a given node at a time.
    log_captures: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

/// RAII marker returned by `RunRegistry::begin_action`: removes `node_id`
/// from `pending` when the call it guards finishes, success or error, so a
/// later — not concurrent — action against the same node is never blocked.
struct PendingGuard<'a> {
    pending: &'a Mutex<HashMap<String, PendingAction>>,
    node_id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.node_id);
    }
}

impl RunRegistry {
    /// Loads any runs persisted from a previous `fghjd` lifetime and
    /// reconciles each against real docker state: a run whose containers are
    /// all still alive is restored with freshly-inspected statuses, and a run
    /// missing any container (removed out-of-band, or lost across a reboot
    /// with no restart policy) is dropped rather than presented as running.
    pub async fn new(
        workspace: std::path::PathBuf,
        db: Arc<WorkspaceDb>,
        docker: Arc<bollard::Docker>,
    ) -> Result<Self> {
        let reconciled = persistence::rehydrate(db.clone(), docker.clone()).await?;
        Ok(Self {
            workspace,
            db,
            docker,
            runs: Mutex::new(reconciled),
            action_lock: tokio::sync::Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            log_captures: Mutex::new(HashMap::new()),
        })
    }

    /// Starts (or restarts) background capture of `container_name`'s logs
    /// into `db`, opening a fresh generation for `(run_id, node_id)`. Called
    /// from `start_node` — the single choke point every container-creating
    /// path (`start`, `ensure_running`, `restart_container`) already funnels
    /// through — so every container gets its own capture task without each
    /// caller needing to remember to set one up.
    ///
    /// Aborts any capture task already running for this node first: normally
    /// the old task would already have ended on its own (the previous
    /// container's log stream ends when Docker removes it, which
    /// `start_node`'s callers always do before recreating), but aborting
    /// explicitly avoids ever leaving two tasks writing under the same key.
    fn spawn_log_capture(&self, run_id: &str, node_id: &str, container_name: &str) {
        let key = format!("{run_id}:{node_id}");
        let handle = tokio::spawn(capture_container_logs(
            self.db.clone(),
            self.docker.clone(),
            run_id.to_string(),
            node_id.to_string(),
            container_name.to_string(),
        ));
        if let Some(old) = self.log_captures.lock().unwrap().insert(key, handle) {
            old.abort();
        }
    }

    /// Clears out the previous cycle of `action` ("start" or "stop") for
    /// `node_id`, so the step-by-step narration `record_event` appends next
    /// is the only one a caller of the `/events` endpoint sees — per the
    /// explicit "new start overrides old start, new stop overrides old
    /// stop" requirement. Best-effort: a failure here only means a later
    /// `record_event` call might append onto a stale cycle instead of a
    /// fresh one, which isn't worth failing the actual start/stop over.
    async fn begin_event_cycle(&self, run_id: &str, node_id: &str, action: &str) {
        if let Err(e) = self
            .db
            .clone()
            .begin_event_cycle(run_id.to_string(), node_id.to_string(), action.to_string())
            .await
        {
            eprintln!("fghjd: failed to begin {action} event cycle for node {node_id}: {e:#}");
        }
    }

    /// Appends one orchestration-level step (image build, container
    /// creation, healthcheck wait, ...) to the current `action` cycle for
    /// `node_id` — the ArgoCD-style "events" narration of what `fghjd`
    /// itself is doing, distinct from the container's own stdout/stderr
    /// captured by `spawn_log_capture`. Best-effort, same reasoning as
    /// `begin_event_cycle`: a logging failure shouldn't fail the actual
    /// start/stop.
    async fn record_event(
        &self,
        run_id: &str,
        node_id: &str,
        action: &str,
        step: &str,
        status: &str,
        detail: Option<String>,
    ) {
        if let Err(e) = self
            .db
            .clone()
            .append_event(
                run_id.to_string(),
                node_id.to_string(),
                action.to_string(),
                step.to_string(),
                status.to_string(),
                detail,
            )
            .await
        {
            eprintln!("fghjd: failed to record {action} event '{step}' for node {node_id}: {e:#}");
        }
    }

    /// Claims `node_id` for the duration of a start/stop/delete call, or
    /// fails fast if another such call against the same node is already in
    /// flight — see `pending`'s doc comment for why `action_lock` alone
    /// can't provide this. Callers should acquire this *before*
    /// `action_lock`, so a genuine duplicate never even waits in line.
    fn begin_action(&self, node_id: &str, action: PendingAction) -> Result<PendingGuard<'_>> {
        let mut pending = self.pending.lock().unwrap();
        if let Some(existing) = pending.get(node_id) {
            bail!("node {node_id} is already {existing}");
        }
        pending.insert(node_id.to_string(), action);
        drop(pending);
        Ok(PendingGuard {
            pending: &self.pending,
            node_id: node_id.to_string(),
        })
    }

    /// Every live run's state, with each container's `pending_action` filled
    /// in fresh from `pending` — the one place those two otherwise-separate
    /// pieces of state (settled Docker status vs. in-flight action) are
    /// merged into the single view the API and UI actually consume.
    pub fn list(&self) -> Vec<RunState> {
        let pending = self.pending.lock().unwrap();
        self.runs
            .lock()
            .unwrap()
            .values()
            .cloned()
            .map(|mut state| {
                for c in &mut state.containers {
                    c.pending_action = pending.get(&c.node_id).copied();
                }
                state
            })
            .collect()
    }

    /// Re-inspects every live run's containers against real docker state and
    /// updates their recorded status, published port, per-port host
    /// bindings, and routes in place — including flagging any container
    /// that's vanished (e.g. `docker rm`'d by hand, outside fghj) as
    /// `"removed"` — so the next `/runs` poll (and `proxy::serve_https`'s
    /// routing, via `WorkspaceRegistry::resolve_route`) reflects reality
    /// instead of a snapshot frozen at whenever the run last started or was
    /// persisted. This *does* correct for Docker itself moving a container
    /// to a different ephemeral host port (a restart-policy-triggered
    /// restart, or `dockerd` restarting) even though `fghj` never asked for
    /// that restart — but it's still purely observational with respect to
    /// Docker: it only re-reads state Docker already changed on its own, and
    /// never starts, stops, or recreates a container itself.
    ///
    /// Snapshots each container's inspectable identity (name, the port key
    /// `status`/`published_port` are read from, and the container-side ports
    /// `routes` were derived from) while holding the lock, inspects it all
    /// without holding it (inspection is an async docker call per port), then
    /// re-locks to write results back — the lock is never held across an
    /// `.await`.
    pub async fn refresh(&self) {
        let snapshot: Vec<(String, Vec<ContainerInfo>)> = {
            let runs = self.runs.lock().unwrap();
            runs.iter()
                .map(|(run_id, state)| (run_id.clone(), state.containers.clone()))
                .collect()
        };

        let mut results: Vec<(String, Vec<ContainerInfo>)> = Vec::new();
        for (run_id, containers) in snapshot {
            let mut updated = Vec::with_capacity(containers.len());
            for c in containers {
                let inspected = match c.status_port.as_deref() {
                    Some(p) => docker::inspect_status(&self.docker, &c.container_name, p).await,
                    None => docker::inspect_status(&self.docker, &c.container_name, "").await,
                };
                let (status, published_port) = match inspected {
                    Ok(Some(s)) => (s.status, s.published_port),
                    _ => ("removed".to_string(), None),
                };

                // Re-inspects every declared port, not just `status_port` —
                // same "one inspect per remaining port" approach `start_node`
                // uses, reusing the inspect already done above for
                // `status_port`'s own binding rather than re-querying it.
                let mut ports = c.ports.clone();
                for (port, host_port) in ports.iter_mut() {
                    *host_port = if c.status_port.as_deref() == Some(port.as_str()) {
                        published_port
                    } else {
                        docker::inspect_status(&self.docker, &c.container_name, port)
                            .await
                            .ok()
                            .flatten()
                            .and_then(|s| s.published_port)
                    };
                }

                // Each route remembers the container-side port it was
                // derived from (`PortRoute.container_port`), so its
                // `host_port` can be corrected from the freshly re-inspected
                // `ports` map above without needing the original `Node`
                // config back. A route persisted before `container_port`
                // existed (empty string) has no matching key in `ports` and
                // is left as-is — it self-corrects the next time this
                // container is started through `fghj`.
                let routes = c
                    .routes
                    .iter()
                    .cloned()
                    .map(|r| {
                        let host_port = ports
                            .get(&r.container_port)
                            .copied()
                            .flatten()
                            .unwrap_or(r.host_port);
                        PortRoute { host_port, ..r }
                    })
                    .collect();

                updated.push(ContainerInfo {
                    status,
                    published_port,
                    ports,
                    routes,
                    ..c
                });
            }
            results.push((run_id, updated));
        }

        let mut changed_states: Vec<RunState> = Vec::new();
        {
            let mut runs = self.runs.lock().unwrap();
            for (run_id, updated) in results {
                if let Some(state) = runs.get_mut(&run_id) {
                    let mut changed = false;
                    for (c, new) in state.containers.iter_mut().zip(updated) {
                        if c.status != new.status
                            || c.published_port != new.published_port
                            || c.ports != new.ports
                            || c.routes
                                .iter()
                                .map(|r| r.host_port)
                                .ne(new.routes.iter().map(|r| r.host_port))
                        {
                            *c = new;
                            changed = true;
                        }
                    }
                    if changed {
                        changed_states.push(state.clone());
                    }
                }
            }
        }
        for state in changed_states {
            let _ = self.db.clone().save_run(state).await;
        }
    }

    /// The read-only Docker-volume counterpart to `refresh` — lists what
    /// `docker::list_run_volumes` actually finds for `run_id` right now.
    /// Keeps `self.docker` private to this module (nothing outside
    /// `runs.rs` touches the Docker client directly) while still letting
    /// `effects::docker::observe` discover volume identity without its own
    /// independent Docker-polling loop. Empty (rather than an error) if the
    /// Docker call itself fails — same "purely observational, never worth
    /// surfacing as a hard failure" stance as `refresh`.
    pub async fn volume_names(&self, run_id: &str) -> Vec<String> {
        docker::list_run_volumes(&self.docker, run_id)
            .await
            .unwrap_or_default()
    }

    /// A separate, slower-cadence counterpart to `refresh`, driven by
    /// `daemon::spawn_sync_reconciler` rather than the 1-second liveness
    /// loop: recomputes each live container's *desired* hash from the
    /// current `.fghj.yaml` (`resolve_node_spec(..., side_effects: false)`,
    /// so nothing is actually built, pulled, or run) and compares it against
    /// the hash stamped on the container when it was last actually started
    /// through `fghj`, updating `ContainerInfo::synced` in place. `graph` is
    /// re-resolved by the caller on every tick — a config-drift check is
    /// only meaningful against the *current* `.fghj.yaml`, not whatever was
    /// last cached. Purely informational, same as `refresh`: never starts,
    /// stops, or recreates anything itself.
    ///
    /// A node no longer present in `graph` (removed from `.fghj.yaml`
    /// entirely) is reported as `None` ("unknown"), not `false` — there's
    /// nothing to meaningfully compare against, and `false` would
    /// misleadingly read as "confirmed drifted."
    pub async fn refresh_sync_status(&self, graph: &Graph) {
        let snapshot: Vec<(String, Vec<ContainerInfo>)> = {
            let runs = self.runs.lock().unwrap();
            runs.iter()
                .map(|(run_id, state)| (run_id.clone(), state.containers.clone()))
                .collect()
        };

        let mut results: Vec<(String, Vec<Option<bool>>)> = Vec::new();
        for (run_id, containers) in snapshot {
            let mut synced_flags = Vec::with_capacity(containers.len());
            for c in &containers {
                let synced = match graph.nodes.iter().find(|n| n.id == c.node_id) {
                    Some(node) => match self.resolve_node_spec(graph, node, &run_id, false).await {
                        Ok(Some(spec)) => Some(spec_hash(node, &spec) == c.config_hash),
                        Ok(None) | Err(_) => None,
                    },
                    None => None,
                };
                synced_flags.push(synced);
            }
            results.push((run_id, synced_flags));
        }

        let mut changed_states: Vec<RunState> = Vec::new();
        {
            let mut runs = self.runs.lock().unwrap();
            for (run_id, synced_flags) in results {
                if let Some(state) = runs.get_mut(&run_id) {
                    let mut changed = false;
                    for (c, synced) in state.containers.iter_mut().zip(synced_flags) {
                        if c.synced != synced {
                            c.synced = synced;
                            changed = true;
                        }
                    }
                    if changed {
                        changed_states.push(state.clone());
                    }
                }
            }
        }
        for state in changed_states {
            let _ = self.db.clone().save_run(state).await;
        }
    }

    pub fn get(&self, run_id: &str) -> Option<RunState> {
        self.runs.lock().unwrap().get(run_id).cloned()
    }

    pub async fn stop(&self, run_id: &str) -> Result<()> {
        let state = {
            let mut runs = self.runs.lock().unwrap();
            let Some(state) = runs.remove(run_id) else {
                bail!("no such run: {run_id}");
            };
            state
        };
        for c in &state.containers {
            self.begin_event_cycle(run_id, &c.node_id, "stop").await;
            self.record_event(
                run_id,
                &c.node_id,
                "stop",
                "stopping container",
                "running",
                None,
            )
            .await;
            docker::stop_and_remove(&self.docker, &c.container_name).await;
            self.record_event(run_id, &c.node_id, "stop", "stopping container", "ok", None)
                .await;
        }
        if !state.sidecar_container_name.is_empty() {
            docker::stop_and_remove(&self.docker, &state.sidecar_container_name).await;
        }
        let _ = std::fs::remove_dir_all(sidecar_routes_dir(&state.network));
        docker::remove_network(&self.docker, &state.network).await;
        // The default run's `scope: "run"` volumes get the exact same
        // derived name on every start (`derive_volume_name` only folds the
        // run id in for a *named* run) — deleting them here would silently
        // wipe data a plain stop+restart expects to still be there. Only a
        // named/preview run's volumes are safe to clean up.
        if run_id != DEFAULT_RUN_ID {
            docker::remove_run_scoped_volumes(&self.docker, run_id).await;
        }
        self.db.clone().delete_run(run_id.to_string()).await?;
        Ok(())
    }

    /// (Re)starts a single node's container within an already-running run —
    /// the per-node counterpart to `start`/`ensure_running`'s whole-run
    /// granularity, backing the Drawer's "Start" button. Always recreates
    /// from scratch (mirrors `ensure_running`'s stop-then-start dance) so a
    /// `.fghj.yaml` change since the container last started is actually
    /// picked up, rather than silently no-op'ing on an already-running one.
    pub async fn restart_container(
        &self,
        graph: &Graph,
        run_id: &str,
        node_id: &str,
    ) -> Result<ContainerInfo> {
        let _pending = self.begin_action(node_id, PendingAction::Starting)?;
        let _guard = self.action_lock.lock().await;
        let mut state = {
            let runs = self.runs.lock().unwrap();
            let Some(state) = runs.get(run_id) else {
                bail!("no such run: {run_id}");
            };
            state.clone()
        };
        let Some(node) = graph.nodes.iter().find(|n| n.id == node_id) else {
            bail!("no such node: {node_id}");
        };
        let container_name = format!(
            "fghj-{}-{}-{}",
            sanitize_label(&graph.workspace_name),
            run_id,
            sanitize_label(&node.id)
        );
        docker::stop_and_remove(&self.docker, &container_name).await;

        let info = self
            .start_node(
                graph,
                node,
                run_id,
                &state.network,
                state.sidecar_ip.as_deref(),
            )
            .await?;
        state.containers.retain(|c| c.node_id != info.node_id);
        state.containers.push(info.clone());
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs.lock().unwrap().insert(run_id.to_string(), state);
        Ok(info)
    }

    /// Stops a single node's container without removing it or touching the
    /// rest of the run — the Drawer's "Stop" button. Unlike `stop` (whole
    /// run), this leaves the container itself and its named volumes in
    /// place; a subsequent "Start" click just recreates it.
    pub async fn stop_container(&self, run_id: &str, node_id: &str) -> Result<()> {
        let _pending = self.begin_action(node_id, PendingAction::Stopping)?;
        let _guard = self.action_lock.lock().await;
        let mut state = {
            let runs = self.runs.lock().unwrap();
            let Some(state) = runs.get(run_id) else {
                bail!("no such run: {run_id}");
            };
            state.clone()
        };
        let Some(c) = state.containers.iter_mut().find(|c| c.node_id == node_id) else {
            bail!("no such node in run {run_id}: {node_id}");
        };
        self.begin_event_cycle(run_id, node_id, "stop").await;
        self.record_event(
            run_id,
            node_id,
            "stop",
            "stopping container",
            "running",
            None,
        )
        .await;
        docker::stop_container(&self.docker, &c.container_name).await;
        c.status = match docker::inspect_status(&self.docker, &c.container_name, "").await {
            Ok(Some(s)) => s.status,
            _ => "exited".to_string(),
        };
        self.record_event(run_id, node_id, "stop", "stopping container", "ok", None)
            .await;
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs.lock().unwrap().insert(run_id.to_string(), state);
        Ok(())
    }

    /// Stops and removes a single node's container, dropping it from the
    /// run entirely — the Drawer's "Delete" button. Named volumes survive
    /// (same reasoning as `stop`'s default-run carve-out: a volume's whole
    /// point is to outlive any one container), so a later "Start" click
    /// picks the data back up in a fresh container.
    pub async fn remove_container(&self, run_id: &str, node_id: &str) -> Result<()> {
        let _pending = self.begin_action(node_id, PendingAction::Removing)?;
        let _guard = self.action_lock.lock().await;
        let mut state = {
            let runs = self.runs.lock().unwrap();
            let Some(state) = runs.get(run_id) else {
                bail!("no such run: {run_id}");
            };
            state.clone()
        };
        let Some(pos) = state.containers.iter().position(|c| c.node_id == node_id) else {
            bail!("no such node in run {run_id}: {node_id}");
        };
        let c = state.containers.remove(pos);
        self.begin_event_cycle(run_id, node_id, "stop").await;
        self.record_event(
            run_id,
            node_id,
            "stop",
            "removing container",
            "running",
            None,
        )
        .await;
        docker::stop_and_remove(&self.docker, &c.container_name).await;
        self.record_event(run_id, node_id, "stop", "removing container", "ok", None)
            .await;
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs.lock().unwrap().insert(run_id.to_string(), state);
        Ok(())
    }

    /// Starts (or confirms already running) this run's in-network TLS proxy
    /// sidecar — one per run, on that run's own docker network, never
    /// shared across workspaces/runs, so a container inside the network can
    /// reach a sibling's `*.fghj.internal` name with the same addressing a
    /// browser outside the network gets from the host-side proxy. Returns
    /// its deterministic container name and its address on `network`.
    ///
    /// Idempotent, like `ensure_running`'s per-node liveness check: checked
    /// directly against Docker rather than trusted from `RunState`, since
    /// the sidecar can be stopped/removed out-of-band just like any other
    /// container.
    async fn ensure_sidecar(
        &self,
        workspace_name: &str,
        run_id: &str,
        network: &str,
    ) -> Result<(String, String)> {
        let name = format!("fghj-{}-{}-sidecar", sanitize_label(workspace_name), run_id);

        let alive = matches!(
            docker::inspect_status(&self.docker, &name, "").await,
            Ok(Some(s)) if s.status == "running"
        );
        if alive && let Some(ip) = docker::inspect_network_ip(&self.docker, &name, network).await? {
            return Ok((name, ip));
        }
        // A stopped-but-not-removed sidecar from a previous run would
        // otherwise collide with create_container's fixed name.
        docker::stop_and_remove(&self.docker, &name).await;

        crate::sidecar_image::ensure_built(&self.docker).await?;

        let routes_dir = sidecar_routes_dir(network);
        std::fs::create_dir_all(&routes_dir)
            .with_context(|| format!("failed to create {routes_dir:?}"))?;
        let routes_path = sidecar_routes_path(network);
        if !routes_path.exists() {
            std::fs::write(&routes_path, b"[]")
                .with_context(|| format!("failed to create {routes_path:?}"))?;
        }

        // Canonicalized, not the literal `/var/lib/...` path: macOS's `/var`
        // is a symlink to `/private/var`, and OrbStack's bind-mount source
        // resolution doesn't follow it — a source of `/var/lib/fghjd/...`
        // silently resolves inside the Docker VM's own filesystem instead of
        // the real host path, so the mounted directory shows up empty
        // instead of erroring. The already-resolved `/private/var/lib/...`
        // form mounts correctly.
        let routes_dir = std::fs::canonicalize(&routes_dir)
            .with_context(|| format!("failed to canonicalize {routes_dir:?}"))?;
        let ca_dir = refresh_sidecar_ca_copy()?;
        let ca_dir = std::fs::canonicalize(ca_dir)
            .context("failed to canonicalize the sidecar CA directory")?;

        // Sibling mounts, not nested — binding `ca` underneath an already
        // bind-mounted, read-only `/etc/fghj-sidecar` fails outright (the
        // container runtime can't create a mountpoint inside a read-only
        // mount). `/etc/fghj-sidecar` itself is never bind-mounted from the
        // host, so the runtime creates it as an ordinary (writable)
        // directory in the container's own layer, and both binds attach
        // under it independently.
        let binds = vec![
            format!("{}:/etc/fghj-sidecar/routes:ro", routes_dir.display()),
            format!("{}:/etc/fghj-sidecar/ca:ro", ca_dir.display()),
        ];

        docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name: &name,
                network,
                aliases: &[],
                env: &[],
                ports: &[],
                image: &crate::sidecar_image::image_tag(),
                command: &[],
                project: network,
                service_name: "fghj-sidecar",
                binds: &binds,
                restart_policy: "unless-stopped",
                user: None,
                working_dir: None,
                labels: &BTreeMap::new(),
                cap_add: &[],
                cap_drop: &[],
                privileged: false,
                extra_hosts: &[],
                dns: &[],
                healthcheck: None,
                platform: None,
            },
        )
        .await
        .context("failed to start this run's sidecar proxy")?;

        let ip = docker::inspect_network_ip(&self.docker, &name, network)
            .await?
            .context("sidecar proxy started but has no address on its own network")?;
        Ok((name, ip))
    }

    pub async fn start(&self, graph: &Graph, spec: RunSpec) -> Result<RunState> {
        let run_id = resolve_run_id(spec.run_id.as_deref());

        // starting an already-running run replaces it cleanly
        let already_running = self.runs.lock().unwrap().contains_key(&run_id);
        if already_running {
            self.stop(&run_id).await?;
        }

        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;

        let (sidecar_container_name, sidecar_ip) = match self
            .ensure_sidecar(&graph.workspace_name, &run_id, &network)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                docker::remove_network(&self.docker, &network).await;
                return Err(e);
            }
        };

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| {
                spec.flow
                    .as_deref()
                    .is_none_or(|flow| n.flows.iter().any(|f| f == flow))
            })
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);

        let mut containers = Vec::new();
        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            match self
                .start_node(graph, node, &run_id, &network, Some(&sidecar_ip))
                .await
            {
                Ok(info) => {
                    containers.push(info);
                }
                Err(e) => {
                    for c in &containers {
                        docker::stop_and_remove(&self.docker, &c.container_name).await;
                    }
                    docker::stop_and_remove(&self.docker, &sidecar_container_name).await;
                    docker::remove_network(&self.docker, &network).await;
                    return Err(e);
                }
            }
        }

        let state = RunState {
            run_id: run_id.clone(),
            network,
            containers,
            sidecar_container_name,
            sidecar_ip: Some(sidecar_ip),
        };
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs.lock().unwrap().insert(run_id, state.clone());
        Ok(state)
    }

    /// Tops up the single default environment so every node reachable from
    /// `flow` (or every node in the graph, if `flow` is `None`) is running —
    /// unlike `start`, this never touches a container that's already alive.
    /// fghj models one shared set of running containers per workspace, not a
    /// separate environment per flow, so picking a flow should never restart
    /// (or duplicate) whatever's already up.
    ///
    /// Liveness is checked directly against docker on every call rather than
    /// trusting the persisted `RunState`, since a container can be
    /// stopped/removed out-of-band between calls (see `refresh`).
    pub async fn ensure_running(&self, graph: &Graph, flow: Option<&str>) -> Result<RunState> {
        let run_id = DEFAULT_RUN_ID.to_string();
        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;
        let (sidecar_container_name, sidecar_ip) = self
            .ensure_sidecar(&graph.workspace_name, &run_id, &network)
            .await?;

        let mut state = self
            .runs
            .lock()
            .unwrap()
            .get(&run_id)
            .cloned()
            .unwrap_or_else(|| RunState {
                run_id: run_id.clone(),
                network: network.clone(),
                containers: Vec::new(),
                sidecar_container_name: sidecar_container_name.clone(),
                sidecar_ip: Some(sidecar_ip.clone()),
            });
        state.sidecar_container_name = sidecar_container_name;
        state.sidecar_ip = Some(sidecar_ip);
        // Persisted unconditionally, not just when a node below actually
        // needs (re)starting — otherwise a call where every node is already
        // alive would compute a fresh sidecar IP but never actually publish
        // it into `self.runs`/the DB/the route table, leaving a later
        // `restart_container` to see a stale `sidecar_ip: None`.
        self.db.clone().save_run(state.clone()).await?;
        if let Err(e) = write_route_table(&state) {
            eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
        }
        self.runs
            .lock()
            .unwrap()
            .insert(run_id.clone(), state.clone());

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| flow.is_none_or(|flow| n.flows.iter().any(|f| f == flow)))
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);

        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            let container_name = format!(
                "fghj-{}-{}-{}",
                sanitize_label(&graph.workspace_name),
                run_id,
                sanitize_label(&node.id)
            );
            let alive = matches!(
                docker::inspect_status(&self.docker, &container_name, "").await,
                Ok(Some(s)) if s.status == "running"
            );
            // Alive alone isn't enough to skip: `state.containers` (loaded
            // from `self.runs`, itself loaded from the DB — see `new`'s
            // reconciliation, which drops a whole run's history the moment
            // any single one of its containers isn't found) can be missing
            // this node's `ContainerInfo`/routes even though the container
            // itself is still running fine. Falling through and recreating
            // it is how it gets re-described (and its routes re-registered
            // in the route table below) rather than staying silently
            // unrouted until something else happens to bounce it.
            if alive && state.containers.iter().any(|c| c.node_id == node.id) {
                continue;
            }
            // A stopped-but-not-removed container from a previous run would
            // otherwise collide with create_container's fixed name.
            docker::stop_and_remove(&self.docker, &container_name).await;

            let info = self
                .start_node(graph, node, &run_id, &network, state.sidecar_ip.as_deref())
                .await?;
            state.containers.retain(|c| c.node_id != info.node_id);
            state.containers.push(info);
            // Saved after every node, not just at the end, so a later
            // failure in this same call doesn't lose track of containers
            // that did start successfully.
            self.db.clone().save_run(state.clone()).await?;
            if let Err(e) = write_route_table(&state) {
                eprintln!("fghjd: failed to write sidecar route table for run {run_id}: {e:#}");
            }
            self.runs
                .lock()
                .unwrap()
                .insert(run_id.clone(), state.clone());
        }

        Ok(state)
    }

    /// The pure, side-effect-free half of resolving a node's config — image
    /// tag, env, port list, volume binds, domain aliases — shared between
    /// `start_node` (`side_effects: true`, which then actually builds the
    /// image / ensures named volumes exist / runs the container) and
    /// `refresh_sync_status` (`side_effects: false`, which only needs the
    /// same values to compute a comparable hash — see `spec_hash` — without
    /// building anything or touching Docker at all).
    async fn resolve_node_spec(
        &self,
        graph: &Graph,
        node: &Node,
        run_id: &str,
        side_effects: bool,
    ) -> Result<Option<NodeSpec>> {
        let workspace = sanitize_label(&graph.workspace_name);
        let container_name = format!("fghj-{workspace}-{run_id}-{}", sanitize_label(&node.id));
        // Every node's domain is derived the same way, unconditionally —
        // there's no CUE-declared override for any node kind (services
        // included) that could bypass this, so two nodes can never collide
        // on a name the way a hand-written one could. Built from `node.id`
        // rather than `node.label`: `id` is a unique, leaf-first dotted
        // chain (`dep-name.owning-service-id` for backing deps — see
        // `resolver::visit_dependency` — or `service-name.repo-local-path`
        // for services — see `resolver::visit_local_service`), while `label`
        // is only the bare declared name and can collide, e.g. when two
        // peer repos each declare a same-named service, or two different
        // services each own their own same-named backing dependency.
        // `run_id` is folded in just like it is for
        // `container_name`/the network name above, *except* for the default
        // run: fghj models one shared, singular default environment per
        // workspace (see `ensure_running`), so it needs no disambiguating
        // segment — only a named/review run does, since more than one of
        // those can be alive at once. `node.domain_scope == "stable"` is the
        // other opt-out (CUE `#Service.domain_scope` /
        // `#BackingDependency.domain_scope`): a deliberate, explicit choice
        // by the CUE author to give a node one fixed identity shared across
        // every run, not just the default one.
        //
        // `domain` (the `fghj.internal` zone) is never registered as a
        // Docker alias on this node's own container — the run's sidecar
        // owns resolving it, universally, from inside this run's docker
        // network (see `dns.rs`'s module doc for the full zone split), so
        // it stays consistent with what the host's own DNS server answers
        // it with too. `raw_domain` (`fghj.raw.internal`) is the real
        // Docker network alias registered below: in-network-only, resolved
        // straight to this container's own IP by Docker's embedded DNS.
        let domain = derive_domain(
            &node.id,
            &node.domain_scope,
            &graph.workspace_name,
            run_id,
            DomainZone::Http,
        );
        let raw_domain = derive_domain(
            &node.id,
            &node.domain_scope,
            &graph.workspace_name,
            run_id,
            DomainZone::Raw,
        );

        // Where a node's relative bind-mount `host` / `env_file` paths
        // resolve against — the repo's checkout root, not `build.context`
        // (Compose resolves both relative to the compose file's directory;
        // this is the fghj equivalent). For a service, its own checkout
        // root; for a backing dependency, which has no checkout of its own,
        // the *owning* service's checkout root (set below, via the graph's
        // "owns" edge).
        let mut volume_base: Option<PathBuf> = None;

        let image = match node.kind.as_str() {
            "backing" => {
                // A backing dependency has no checkout of its own to resolve
                // a relative `env_file` (or bind-mount `host`) path against —
                // same rule Compose uses, resolving `env_file` against the
                // compose file's own directory regardless of `build` vs
                // `image`. Its equivalent of "the compose file's directory"
                // is the *owning* service's checkout root: the service whose
                // .fghj.yaml declares this dependency inline, found via the
                // graph's "owns" edge (`resolver::visit_dependency` always
                // pushes owner -> backing).
                let owner_local_path = graph
                    .edges
                    .iter()
                    .find(|e| e.kind == "owns" && e.to == node.id)
                    .and_then(|e| graph.nodes.iter().find(|n| n.id == e.from))
                    .and_then(|n| n.local_path.as_ref());
                if let Some(owner_local_path) = owner_local_path {
                    volume_base = Some(self.workspace.join(owner_local_path));
                }
                match node.image.clone() {
                    Some(img) => img,
                    None => bail!("backing node {} has no image", node.id),
                }
            }
            _ => {
                let build = node.build.clone().unwrap_or(crate::resolver::NodeBuild {
                    context: ".".to_string(),
                    dockerfile: "Dockerfile".to_string(),
                    args: BTreeMap::new(),
                });

                let local_path = match node.local_path.clone() {
                    Some(p) => p,
                    None => bail!("service node {} has no local_path", node.id),
                };
                let branch = node.branch.clone().unwrap_or_else(|| "local".to_string());
                let tag = format!(
                    "fghj/{}:{}",
                    sanitize_label(&node.id),
                    sanitize_label(&branch)
                );
                let repo_root = self.workspace.join(&local_path);
                volume_base = Some(repo_root.clone());
                if side_effects {
                    let build_dir = repo_root.join(&build.context);
                    self.record_event(
                        run_id,
                        &node.id,
                        "start",
                        "building image",
                        "running",
                        Some(tag.clone()),
                    )
                    .await;
                    if let Err(e) = docker::build_image(
                        &self.docker,
                        &build_dir,
                        &build.dockerfile,
                        &tag,
                        node.platform.as_deref(),
                    )
                    .await
                    {
                        self.record_event(
                            run_id,
                            &node.id,
                            "start",
                            "building image",
                            "error",
                            Some(format!("{e:#}")),
                        )
                        .await;
                        return Err(e);
                    }
                    self.record_event(run_id, &node.id, "start", "building image", "ok", None)
                        .await;
                }
                tag
            }
        };

        // Named ports (`#Port.name`) get their own domain, nested under this
        // node's raw domain — `admin.api.default.shop.fghj.raw.internal` —
        // and need to be real Docker aliases too, so a sibling container can
        // reach a specific named port directly by name instead of having to
        // know its container-side port number ahead of time.
        let mut aliases = vec![raw_domain.clone()];
        aliases.extend(
            node.ports
                .values()
                .filter_map(|p| p.name.as_ref())
                .map(|name| format!("{name}.{raw_domain}")),
        );

        let port_list: Vec<(String, Option<u16>)> = node
            .ports
            .iter()
            .map(|(port, cfg)| (port.clone(), cfg.host_port))
            .collect();

        // A named volume's Docker-side existence is otherwise implicit (the
        // daemon auto-creates one, unlabeled, the first time a bind
        // references it) — `ensure_volume` here labels it so
        // `docker::remove_run_scoped_volumes` can find it in `stop()`.
        // `Iterator::map` can't `.await`, hence the explicit loop.
        let mut binds: Vec<String> = Vec::with_capacity(node.volumes.len());
        for v in &node.volumes {
            match v {
                VolumeMount::Bind {
                    host,
                    container,
                    read_only,
                } => {
                    let host_path = if Path::new(host).is_absolute() {
                        PathBuf::from(host)
                    } else {
                        volume_base
                            .as_ref()
                            .expect("node with volumes has a resolved checkout root")
                            .join(host)
                    };
                    // Resolve symlinks (notably macOS's `/var` -> `/private/var`)
                    // before handing the path to Docker: bind-mounting a file
                    // through a symlinked parent directory makes some Docker
                    // backends (OrbStack) misdetect the mount source's type and
                    // reject an otherwise-valid file-to-file bind mount.
                    let host_path = std::fs::canonicalize(&host_path).unwrap_or(host_path);
                    binds.push(format!(
                        "{}:{container}{}",
                        host_path.display(),
                        if *read_only { ":ro" } else { "" }
                    ));
                }
                VolumeMount::Named {
                    name,
                    scope,
                    container,
                    read_only,
                } => {
                    let volume_name =
                        derive_volume_name(name, scope, &graph.workspace_name, run_id);
                    if side_effects {
                        docker::ensure_volume(
                            &self.docker,
                            &volume_name,
                            &graph.workspace_name,
                            scope,
                            run_id,
                        )
                        .await?;
                    }
                    binds.push(format!(
                        "{volume_name}:{container}{}",
                        if *read_only { ":ro" } else { "" }
                    ));
                }
            }
        }

        // `env_file` entries load first, in declared order, then
        // `environment` is applied on top — same precedence as Compose,
        // relying on Docker's own last-value-wins behavior for a flat `-e`
        // list rather than de-duping keys here. Relative paths resolve
        // against `volume_base` — this node's own checkout root for a
        // service, or the owning service's for a backing dependency.
        let mut env: Vec<String> = Vec::new();
        for path in &node.env_file {
            let file_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                volume_base
                    .as_ref()
                    .expect("node with env_file has a resolved checkout root")
                    .join(path)
            };
            let contents = std::fs::read_to_string(&file_path)
                .with_context(|| format!("failed to read env_file {}", file_path.display()))?;
            env.extend(parse_env_file(&contents));
        }
        env.extend(node.environment.iter().cloned());
        for entry in &mut env {
            *entry =
                expand_service_fqdn_templates(entry, node, &raw_domain, &domain, graph, run_id);
        }

        Ok(Some(NodeSpec {
            container_name,
            domain,
            raw_domain,
            aliases,
            image,
            port_list,
            binds,
            env,
        }))
    }

    /// Actually starts a node's container: resolves its full spec via
    /// `resolve_node_spec` (`side_effects: true`, so the image gets built
    /// and named volumes get created along the way), stamps the resulting
    /// `spec_hash` onto the container as a `fghj.config_hash` label, runs
    /// it, and inspects the result. That label — and the copy of the same
    /// hash returned on `ContainerInfo::config_hash` — is what a later
    /// `refresh_sync_status` pass compares a freshly recomputed desired hash
    /// against to decide whether this node has drifted since it was last
    /// started.
    async fn start_node(
        &self,
        graph: &Graph,
        node: &Node,
        run_id: &str,
        network: &str,
        sidecar_ip: Option<&str>,
    ) -> Result<ContainerInfo> {
        self.begin_event_cycle(run_id, &node.id, "start").await;
        self.record_event(
            run_id,
            &node.id,
            "start",
            "resolving config",
            "running",
            None,
        )
        .await;
        let spec = match self.resolve_node_spec(graph, node, run_id, true).await {
            Ok(Some(spec)) => spec,
            Ok(None) => {
                let msg = format!(
                    "resolve_node_spec returned no spec for {} despite side_effects being enabled",
                    node.id
                );
                self.record_event(
                    run_id,
                    &node.id,
                    "start",
                    "resolving config",
                    "error",
                    Some(msg.clone()),
                )
                .await;
                bail!(msg);
            }
            Err(e) => {
                self.record_event(
                    run_id,
                    &node.id,
                    "start",
                    "resolving config",
                    "error",
                    Some(format!("{e:#}")),
                )
                .await;
                return Err(e);
            }
        };
        self.record_event(run_id, &node.id, "start", "resolving config", "ok", None)
            .await;
        let config_hash = spec_hash(node, &spec);
        let mut labels = node.labels.clone();
        labels.insert("fghj.config_hash".to_string(), config_hash.clone());

        // Every node asks this run's sidecar for DNS first — it's the
        // authority for the `fghj.internal` zone (and any active
        // `additional_hosts`/`wildcard_hosts` alias) inside this network,
        // forwarding anything else on to Docker's own embedded resolver.
        // Falls back to Docker's default (unset) only if the sidecar's IP
        // somehow isn't known yet — `ensure_sidecar` always runs, and is
        // inspected for its IP, before any node's `start_node` call, so
        // this shouldn't actually happen in practice.
        let dns: Vec<String> = sidecar_ip
            .map(|ip| vec![ip.to_string(), "127.0.0.11".to_string()])
            .unwrap_or_default();

        self.record_event(
            run_id,
            &node.id,
            "start",
            "creating container",
            "running",
            None,
        )
        .await;
        if let Err(e) = docker::run_container(
            &self.docker,
            &docker::RunOpts {
                name: &spec.container_name,
                network,
                aliases: &spec.aliases,
                env: &spec.env,
                dns: &dns,
                ports: &spec.port_list,
                image: &spec.image,
                command: &node.command,
                project: network,
                service_name: &node.id,
                binds: &spec.binds,
                restart_policy: &node.restart,
                user: node.user.as_deref(),
                working_dir: node.working_dir.as_deref(),
                labels: &labels,
                cap_add: &node.cap_add,
                cap_drop: &node.cap_drop,
                privileged: node.privileged,
                extra_hosts: &node.extra_hosts,
                healthcheck: node.healthcheck.as_ref(),
                platform: node.platform.as_deref(),
            },
        )
        .await
        {
            self.record_event(
                run_id,
                &node.id,
                "start",
                "creating container",
                "error",
                Some(format!("{e:#}")),
            )
            .await;
            return Err(e);
        }
        self.record_event(run_id, &node.id, "start", "creating container", "ok", None)
            .await;

        self.spawn_log_capture(run_id, &node.id, &spec.container_name);

        // Prefer the port explicitly marked `primary` — the one actually
        // meant to be "the" entrypoint — over an arbitrary map-iteration
        // order (a `BTreeMap<String, _>` sorts port numbers as strings, so
        // e.g. "10000" would otherwise sort before "9000").
        let status_port = node
            .ports
            .iter()
            .find(|(_, cfg)| cfg.primary)
            .map(|(port, _)| port.clone())
            .or_else(|| node.ports.keys().next().cloned());
        let inspected = match &status_port {
            Some(p) => docker::inspect_status(&self.docker, &spec.container_name, p).await?,
            None => docker::inspect_status(&self.docker, &spec.container_name, "").await?,
        };
        let (status, published_port) = match inspected {
            Some(s) => (s.status, s.published_port),
            None => ("unknown".to_string(), None),
        };

        // Every declared port's actual host-published binding, not just the
        // routed ones — a plain TCP backing dependency (postgres, mysql)
        // has no `primary`/`name`d port to route at all, but the UI still
        // wants a `127.0.0.1:<port>` connection string for it. Reuses the
        // inspect already done above for `status_port`'s own binding rather
        // than re-querying it; one more inspect per remaining port (there's
        // rarely more than one or two per node).
        let mut port_host_ports: BTreeMap<String, Option<u16>> = BTreeMap::new();
        for port in node.ports.keys() {
            let host_port = if status_port.as_deref() == Some(port.as_str()) {
                published_port
            } else {
                docker::inspect_status(&self.docker, &spec.container_name, port)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.published_port)
            };
            port_host_ports.insert(port.clone(), host_port);
        }

        // Every port with a domain — the `primary` one, at this node's own
        // domain, and/or any `name`d one, at `{name}.{domain}` (a port can
        // be both) — gets a route to the host port Docker actually
        // published it on. `fghjd` runs on the host, not inside the docker
        // network, so it can't resolve these names the way sibling
        // containers do (via Docker's embedded per-network DNS, which only
        // answers from inside that network) — this is what lets
        // `proxy::serve_https` dispatch an incoming SNI straight to the
        // right container instead.
        let mut routes = Vec::new();
        for (port, cfg) in &node.ports {
            if !cfg.primary && cfg.name.is_none() {
                continue;
            }
            let Some(host_port) = port_host_ports.get(port).copied().flatten() else {
                continue;
            };
            if cfg.primary {
                routes.push(PortRoute {
                    https: dns::cert_eligible(&spec.domain, true),
                    domain: spec.domain.clone(),
                    host_port,
                    wildcard: cfg.wildcard,
                    container_port: port.clone(),
                });
            }
            if let Some(name) = &cfg.name {
                let domain = format!("{name}.{}", spec.domain);
                routes.push(PortRoute {
                    https: dns::cert_eligible(&domain, true),
                    domain,
                    host_port,
                    wildcard: cfg.wildcard,
                    container_port: port.clone(),
                });
            }
        }

        // `#AdditionalHost` aliases route to the same host port as the
        // node's own primary domain — same "multiple names, one backend
        // port" pattern as a named port, just keyed off a literal
        // author-declared hostname instead of a derived one. Silently
        // dropped (not an error) if there's no primary-port route to attach
        // to — `resolver::check_ports` already warns about exactly this at
        // graph-resolution time.
        let mut additional_hosts_active = Vec::new();
        if let Some((host_port, container_port)) = routes
            .iter()
            .find(|r| r.domain == spec.domain)
            .map(|r| (r.host_port, r.container_port.clone()))
        {
            for host in &node.additional_hosts {
                routes.push(PortRoute {
                    https: dns::cert_eligible(host, true),
                    domain: host.clone(),
                    host_port,
                    wildcard: false,
                    container_port: container_port.clone(),
                });
                additional_hosts_active.push(host.clone());
            }
            for suffix in &node.wildcard_hosts {
                routes.push(PortRoute {
                    https: dns::cert_eligible(suffix, true),
                    domain: suffix.clone(),
                    host_port,
                    wildcard: true,
                    container_port: container_port.clone(),
                });
            }
        }

        if node.healthcheck.is_some() {
            self.record_event(
                run_id,
                &node.id,
                "start",
                "waiting for healthcheck",
                "running",
                None,
            )
            .await;
            wait_for_healthy(&self.docker, &spec.container_name).await;
            self.record_event(
                run_id,
                &node.id,
                "start",
                "waiting for healthcheck",
                "ok",
                None,
            )
            .await;
        }
        self.record_event(run_id, &node.id, "start", "ready", "ok", None)
            .await;

        Ok(ContainerInfo {
            node_id: node.id.clone(),
            container_name: spec.container_name,
            status,
            published_port,
            domain: spec.domain,
            raw_domain: spec.raw_domain,
            routes,
            additional_hosts: additional_hosts_active,
            ports: port_host_ports,
            status_port,
            config_hash,
            synced: Some(true),
            pending_action: None,
        })
    }
}

pub async fn logs_for_tail(
    docker: &bollard::Docker,
    state: &RunState,
    node_id: &str,
    tail: usize,
) -> Result<String> {
    let Some(c) = state.containers.iter().find(|c| c.node_id == node_id) else {
        bail!("no such node in run: {node_id}");
    };
    docker::logs_tail(docker, &c.container_name, tail).await
}

/// Returns the container name backing `node_id` in `state`, for the SSE
/// live-follow endpoint to build a `docker::logs_follow` stream from.
pub fn container_name_for<'a>(state: &'a RunState, node_id: &str) -> Result<&'a str> {
    state
        .containers
        .iter()
        .find(|c| c.node_id == node_id)
        .map(|c| c.container_name.as_str())
        .ok_or_else(|| anyhow::anyhow!("no such node in run: {node_id}"))
}

/// Runs for the lifetime of one container: opens a fresh log generation,
/// then streams `container_name`'s stdout/stderr into it via
/// `docker::logs_follow` until the stream ends (the container stops or is
/// removed), flushing periodically so a live tail stays close to real-time
/// and unconditionally at the end so a crash's final lines are never lost —
/// the whole point of persisting logs in the first place, since Docker's own
/// retained logs are destroyed the moment `stop_and_remove` runs.
///
/// Each stream (stdout/stderr) gets its own leftover buffer so a line split
/// across two chunks is reassembled correctly without stdout/stderr
/// interleaving corrupting each other's partial line.
async fn capture_container_logs(
    db: Arc<WorkspaceDb>,
    docker: Arc<bollard::Docker>,
    run_id: String,
    node_id: String,
    container_name: String,
) {
    let generation = match db
        .clone()
        .begin_log_generation(run_id.clone(), node_id.clone())
        .await
    {
        Ok(g) => g,
        Err(e) => {
            eprintln!("fghjd: failed to begin log generation for node {node_id}: {e:#}");
            return;
        }
    };

    let mut stream = docker::logs_follow(&docker, &container_name, true);
    let mut seq: i64 = 0;
    let mut buffer: Vec<LogLine> = Vec::new();
    let mut leftover_out = String::new();
    let mut leftover_err = String::new();
    let mut last_flush = std::time::Instant::now();

    while let Some(item) = stream.next().await {
        let (is_err, message) = match item {
            Ok(bollard::container::LogOutput::StdOut { message }) => (false, message),
            Ok(bollard::container::LogOutput::Console { message }) => (false, message),
            Ok(bollard::container::LogOutput::StdErr { message }) => (true, message),
            Ok(bollard::container::LogOutput::StdIn { .. }) => continue,
            Err(_) => break,
        };
        let leftover = if is_err {
            &mut leftover_err
        } else {
            &mut leftover_out
        };
        leftover.push_str(&String::from_utf8_lossy(&message));
        while let Some(pos) = leftover.find('\n') {
            let raw_line: String = leftover.drain(..=pos).collect();
            let raw_line = raw_line.trim_end_matches('\n');
            let (ts, line) = raw_line.split_once(' ').unwrap_or(("", raw_line));
            buffer.push(LogLine {
                seq,
                stream: if is_err { "stderr" } else { "stdout" }.to_string(),
                ts: ts.to_string(),
                line: line.to_string(),
            });
            seq += 1;
        }
        if buffer.len() >= 200 || last_flush.elapsed() >= Duration::from_millis(500) {
            let batch = std::mem::take(&mut buffer);
            let _ = db
                .clone()
                .insert_log_lines(run_id.clone(), node_id.clone(), generation, batch)
                .await;
            last_flush = std::time::Instant::now();
        }
    }

    for (is_err, leftover) in [(false, &mut leftover_out), (true, &mut leftover_err)] {
        if !leftover.is_empty() {
            let raw_line = std::mem::take(leftover);
            let (ts, line) = raw_line.split_once(' ').unwrap_or(("", raw_line.as_str()));
            buffer.push(LogLine {
                seq,
                stream: if is_err { "stderr" } else { "stdout" }.to_string(),
                ts: ts.to_string(),
                line: line.to_string(),
            });
            seq += 1;
        }
    }

    if !buffer.is_empty() {
        let _ = db
            .insert_log_lines(run_id, node_id, generation, buffer)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_label_lowercases_and_collapses_separators() {
        assert_eq!(
            sanitize_label("Feature/JIRA-123 Fix"),
            "feature-jira-123-fix"
        );
        assert_eq!(sanitize_label("already-clean"), "already-clean");
        assert_eq!(sanitize_label("__leading__"), "leading");
    }

    #[test]
    fn resolve_run_id_falls_back_to_default_when_absent_or_empty() {
        assert_eq!(resolve_run_id(None), DEFAULT_RUN_ID);
        assert_eq!(resolve_run_id(Some("")), DEFAULT_RUN_ID);
        assert_eq!(resolve_run_id(Some("///")), DEFAULT_RUN_ID);
    }

    #[test]
    fn resolve_run_id_sanitizes_a_given_id() {
        assert_eq!(resolve_run_id(Some("Feature/JIRA-123")), "feature-jira-123");
    }

    #[test]
    fn derive_domain_picks_the_suffix_for_the_requested_zone() {
        assert_eq!(
            derive_domain("svc", "run", "demo", DEFAULT_RUN_ID, DomainZone::Http),
            "svc.demo.fghj.internal"
        );
        assert_eq!(
            derive_domain("svc", "run", "demo", DEFAULT_RUN_ID, DomainZone::Raw),
            "svc.demo.fghj.raw.internal"
        );
        // Non-default run id, non-stable scope: run id folds into both zones
        // identically, only the suffix differs.
        assert_eq!(
            derive_domain("svc", "run", "demo", "feature-x", DomainZone::Http),
            "svc.feature-x.demo.fghj.internal"
        );
        assert_eq!(
            derive_domain("svc", "run", "demo", "feature-x", DomainZone::Raw),
            "svc.feature-x.demo.fghj.raw.internal"
        );
        // `stable` scope folds out the run id in both zones the same way.
        assert_eq!(
            derive_domain("svc", "stable", "demo", "feature-x", DomainZone::Raw),
            "svc.demo.fghj.raw.internal"
        );
    }

    #[test]
    fn route_file_entries_connect_via_the_raw_domain_not_the_http_one() {
        let containers = vec![ContainerInfo {
            node_id: "svc".to_string(),
            container_name: "fghj-test-svc".to_string(),
            status: "running".to_string(),
            published_port: Some(8080),
            domain: "svc.demo.fghj.internal".to_string(),
            raw_domain: "svc.demo.fghj.raw.internal".to_string(),
            routes: vec![PortRoute {
                domain: "svc.demo.fghj.internal".to_string(),
                host_port: 8080,
                wildcard: false,
                https: true,
                container_port: "8080".to_string(),
            }],
            additional_hosts: Vec::new(),
            ports: BTreeMap::from([("8080".to_string(), Some(8080))]),
            status_port: Some("8080".to_string()),
            config_hash: String::new(),
            synced: None,
            pending_action: None,
        }];

        let entries = route_file_entries(&containers);
        assert_eq!(
            entries,
            vec![RouteFileEntry {
                lookup: "svc.demo.fghj.internal".to_string(),
                wildcard: false,
                connect_host: "svc.demo.fghj.raw.internal".to_string(),
                connect_port: 8080,
            }]
        );
    }

    fn edge(from: &str, to: &str, kind: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind: kind.to_string(),
            branch: None,
            flows: Vec::new(),
        }
    }

    #[test]
    fn topological_start_order_places_dependencies_before_dependents() {
        let ids = vec!["app".to_string(), "db".to_string()];
        let edges = vec![edge("app", "db", "owns")];
        let order = topological_start_order(&ids, &edges);
        let db_pos = order.iter().position(|id| id == "db").unwrap();
        let app_pos = order.iter().position(|id| id == "app").unwrap();
        assert!(db_pos < app_pos);
    }

    #[test]
    fn topological_start_order_ignores_edges_outside_the_target_set() {
        // A flow-filtered run can exclude a node's dependency entirely —
        // that edge should just be ignored, not panic on a missing id.
        let ids = vec!["app".to_string()];
        let edges = vec![edge("app", "not-in-this-run", "depends-on")];
        let order = topological_start_order(&ids, &edges);
        assert_eq!(order, vec!["app".to_string()]);
    }

    #[test]
    fn topological_start_order_breaks_cycles_instead_of_looping_forever() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let edges = vec![edge("a", "b", "depends-on"), edge("b", "a", "depends-on")];
        let order = topological_start_order(&ids, &edges);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_env_file_skips_blanks_and_comments_and_strips_matching_quotes() {
        let contents =
            "# a comment\n\nFOO=bar\nBAZ=\"quoted value\"\nSINGLE='hi'\nMISMATCHED=\"oops'\n";
        let pairs = parse_env_file(contents);
        assert_eq!(
            pairs,
            vec![
                "FOO=bar",
                "BAZ=quoted value",
                "SINGLE=hi",
                "MISMATCHED=\"oops'",
            ]
        );
    }

    fn test_node(id: &str, label: &str, kind: &str) -> Node {
        Node {
            id: id.to_string(),
            label: label.to_string(),
            kind: kind.to_string(),
            image: None,
            branch: None,
            repo: None,
            domain_scope: "run".to_string(),
            local_path: None,
            domain: String::new(),
            downloaded: true,
            dirty: false,
            flows: Vec::new(),
            build: None,
            ports: BTreeMap::new(),
            environment: Vec::new(),
            command: Vec::new(),
            volumes: Vec::new(),
            additional_hosts: Vec::new(),
            wildcard_hosts: Vec::new(),
            env_file: Vec::new(),
            restart: "no".to_string(),
            user: None,
            working_dir: None,
            labels: BTreeMap::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            privileged: false,
            extra_hosts: Vec::new(),
            healthcheck: None,
            platform: None,
        }
    }

    fn test_graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        Graph {
            workspace_name: "shop".to_string(),
            nodes,
            edges,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_self_reference() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN}/",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "https://php.app.shop.fghj.raw.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_http_variant_resolves_the_proxied_self_reference() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN_HTTP}/",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "https://php.app.shop.fghj.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_sibling_owned_by_a_service() {
        // php owns mysql; php's own environment references its sibling by name.
        let php = test_node("php.app", "php", "service");
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let graph = test_graph(
            vec![php.clone(), mysql],
            vec![edge("php.app", "mysql.php.app", "owns")],
        );
        let out = expand_service_fqdn_templates(
            "mysql://${FGHJ_SERVICE_FQDN:mysql}:3306/app",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "mysql://mysql.php.app.shop.fghj.raw.internal:3306/app");
    }

    #[test]
    fn expand_service_fqdn_templates_http_variant_resolves_a_sibling() {
        let php = test_node("php.app", "php", "service");
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let graph = test_graph(
            vec![php.clone(), mysql],
            vec![edge("php.app", "mysql.php.app", "owns")],
        );
        let out = expand_service_fqdn_templates(
            "https://${FGHJ_SERVICE_FQDN_HTTP:mysql}/",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "https://mysql.php.app.shop.fghj.internal/");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_sibling_owned_by_the_same_owner() {
        // phpmyadmin and mysql are both owned by php; phpmyadmin references
        // its sibling mysql, not anything it owns itself (it owns nothing).
        let php_id = "php.app";
        let mysql = test_node("mysql.php.app", "mysql", "backing");
        let phpmyadmin = test_node("phpmyadmin.php.app", "phpmyadmin", "backing");
        let graph = test_graph(
            vec![mysql, phpmyadmin.clone()],
            vec![
                edge(php_id, "mysql.php.app", "owns"),
                edge(php_id, "phpmyadmin.php.app", "owns"),
            ],
        );
        let out = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:mysql}",
            &phpmyadmin,
            "phpmyadmin.php.app.shop.fghj.raw.internal",
            "phpmyadmin.php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "mysql.php.app.shop.fghj.raw.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_resolves_a_directly_depended_on_sibling_service() {
        // vite depends on php (same-repo `kind: service`) — and the same
        // "depends-on" edge shape covers a cross-repo flow dependency, so
        // this also stands in for that case.
        let vite = test_node("vite.app", "vite", "service");
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(
            vec![vite.clone(), php],
            vec![edge("vite.app", "php.app", "depends-on")],
        );
        let out = expand_service_fqdn_templates(
            "http://${FGHJ_SERVICE_FQDN:php}",
            &vite,
            "vite.app.shop.fghj.raw.internal",
            "vite.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(out, "http://php.app.shop.fghj.raw.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_disambiguates_a_colliding_leaf_name_with_a_path() {
        // php owns a backing dependency named "mysql" *and* directly depends
        // on a cross-repo service that also happens to be named "mysql" —
        // the bare leaf name is ambiguous, so the backing dependency wins by
        // default (declared via "owns", checked first), and the qualified
        // root-first path (mirroring how the id itself, leaf-first, would
        // read as `mysql.otherrepo`) is needed to reach the other one.
        let php = test_node("php.app", "php", "service");
        let mysql_backing = test_node("mysql.php.app", "mysql", "backing");
        let mysql_service = test_node("mysql.otherrepo", "mysql", "service");
        let graph = test_graph(
            vec![php.clone(), mysql_backing, mysql_service],
            vec![
                edge("php.app", "mysql.php.app", "owns"),
                edge("php.app", "mysql.otherrepo", "depends-on"),
            ],
        );

        let bare = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:mysql}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(bare, "mysql.php.app.shop.fghj.raw.internal");

        let qualified = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:otherrepo::mysql}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(qualified, "mysql.otherrepo.shop.fghj.raw.internal");
    }

    #[test]
    fn expand_service_fqdn_templates_leaves_unknown_sibling_and_malformed_token_untouched() {
        let php = test_node("php.app", "php", "service");
        let graph = test_graph(vec![php.clone()], vec![]);

        let unknown = expand_service_fqdn_templates(
            "${FGHJ_SERVICE_FQDN:nope}",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(unknown, "${FGHJ_SERVICE_FQDN:nope}");

        let unterminated = expand_service_fqdn_templates(
            "prefix ${FGHJ_SERVICE_FQDN no closing brace",
            &php,
            "php.app.shop.fghj.raw.internal",
            "php.app.shop.fghj.internal",
            &graph,
            DEFAULT_RUN_ID,
        );
        assert_eq!(unterminated, "prefix ${FGHJ_SERVICE_FQDN no closing brace");
    }

    #[test]
    fn two_nodes_sharing_a_named_volume_derive_the_same_docker_name() {
        // Keyed by the declared `name`, not any node id — two unrelated
        // nodes (service or backing) that declare the same `name` + `scope`
        // land on the same derived value and therefore the same Docker volume.
        let a = derive_volume_name("cache", "run", "shop", "preview-1");
        let b = derive_volume_name("cache", "run", "shop", "preview-1");
        assert_eq!(a, b);

        // "stable" never folds in the run id, so it must differ from a
        // "run"-scoped name for the same non-default run.
        let stable = derive_volume_name("cache", "stable", "shop", "preview-1");
        assert_ne!(a, stable);

        // A different named run gets its own fresh "run"-scoped volume.
        let other_run = derive_volume_name("cache", "run", "shop", "preview-2");
        assert_ne!(a, other_run);
    }

    /// A throwaway container publishing one port to a Docker-picked
    /// ephemeral host port, torn down on drop — exists so `refresh` has a
    /// real container to re-inspect without needing the resolver/`RunOpts`
    /// machinery `start_node` requires.
    struct DriftingPortContainer {
        name: String,
    }

    impl DriftingPortContainer {
        fn start() -> Self {
            let name = format!(
                "fghj-refresh-test-{}",
                std::process::id().wrapping_add(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(0)
                )
            );
            let status = std::process::Command::new("docker")
                .args([
                    "run",
                    "-d",
                    "--rm",
                    "--name",
                    &name,
                    "-p",
                    "127.0.0.1::8080",
                    "busybox",
                    "sleep",
                    "60",
                ])
                .status()
                .expect("failed to run `docker run` for refresh test fixture");
            assert!(
                status.success(),
                "docker run failed for refresh test fixture"
            );
            Self { name }
        }
    }

    impl Drop for DriftingPortContainer {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.name])
                .status();
        }
    }

    /// The bug this guards against: a container Docker republished on a new
    /// ephemeral host port (e.g. after a restart-policy-triggered restart,
    /// outside `fghj`'s own start path) used to leave `status` corrected but
    /// `published_port`/`ports`/`routes` permanently stale, since the old
    /// `refresh` only ever wrote back `status`. Simulates that by seeding
    /// the registry with a route pointing at a deliberately wrong port for a
    /// real, running container, then asserting `refresh` corrects it to the
    /// port Docker actually published.
    #[tokio::test]
    async fn refresh_corrects_a_route_after_docker_moves_the_published_port() {
        let container = DriftingPortContainer::start();
        let docker = Arc::new(crate::daemon::connect_docker().expect("docker client"));
        let real_port = docker::inspect_status(&docker, &container.name, "8080")
            .await
            .expect("inspect_status failed")
            .and_then(|s| s.published_port)
            .expect("container must have a real published port");

        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let registry = RunRegistry::new(tmp.path().to_path_buf(), db, docker)
            .await
            .expect("RunRegistry::new failed");

        let stale_port = real_port.wrapping_add(1).max(1);
        let run_id = "default".to_string();
        let stale_container = ContainerInfo {
            node_id: "svc".to_string(),
            container_name: container.name.clone(),
            status: "running".to_string(),
            published_port: Some(stale_port),
            domain: "svc.demo.fghj.internal".to_string(),
            raw_domain: "svc.demo.fghj.raw.internal".to_string(),
            routes: vec![PortRoute {
                domain: "svc.demo.fghj.internal".to_string(),
                host_port: stale_port,
                wildcard: false,
                https: true,
                container_port: "8080".to_string(),
            }],
            additional_hosts: Vec::new(),
            ports: BTreeMap::from([("8080".to_string(), Some(stale_port))]),
            status_port: Some("8080".to_string()),
            config_hash: String::new(),
            synced: None,
            pending_action: None,
        };
        {
            let mut runs = registry.runs.lock().unwrap();
            runs.insert(
                run_id.clone(),
                RunState {
                    run_id: run_id.clone(),
                    network: "bridge".to_string(),
                    containers: vec![stale_container],
                    sidecar_container_name: String::new(),
                    sidecar_ip: None,
                },
            );
        }

        registry.refresh().await;

        let state = registry.get(&run_id).expect("run must still be tracked");
        let c = &state.containers[0];
        assert_eq!(c.published_port, Some(real_port));
        assert_eq!(c.ports.get("8080").copied().flatten(), Some(real_port));
        assert_eq!(c.routes[0].host_port, real_port);
    }
}
