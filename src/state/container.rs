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

/// Which checkout a container was **built from**, recorded at the moment it
/// was last actually started through fghj.
///
/// `ContainerDesired::config_hash` already makes a moved checkout *visible*:
/// `runs::spec::spec_hash` folds the same `head`/`dirty` pair into the hash,
/// so a commit, pull or rebase flips the node to `Drifted`. What the hash
/// cannot do is say what changed, because it is a digest — it compares equal
/// or unequal and keeps nothing. This keeps the inputs in plain text next to
/// it, so the answer can be "built from `a3f9c1`, checkout is now `4f4cd9d`"
/// rather than only "drifted".
///
/// Deliberately a snapshot, never refreshed: a field updated to follow the
/// checkout would always agree with it and could never show drift, which is
/// the same tautology `ContainerDesired`'s doc comment rules out for
/// `running`. It changes only when the container is recreated.
///
/// `None` on the container for a node fghj does not build, mirroring
/// `spec_hash`'s own `node.build.as_ref().map(..)` gate — an image pulled by
/// tag has no checkout the running code could have drifted from, so there is
/// nothing to report rather than an unknown to display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContainerSource {
    /// The branch checked out when this container was built. For display
    /// only: it is also what the image is tagged with
    /// (`fghj/{id}:{branch}`), so it is the one field here that a human can
    /// cross-check against `docker images`.
    pub branch: Option<String>,
    /// The commit the build saw, full length as git reported it. `None` when
    /// git could not be read at all, which is a different statement from
    /// "no commit" and is why this is not an empty string.
    pub head: Option<String>,
    /// Whether the tree had uncommitted changes at build time.
    ///
    /// Kept despite being the weaker signal, because dropping it would make
    /// the clean -> dirty transition invisible: edit a file without
    /// committing and the container really is serving code that no longer
    /// exists in the checkout. It is one bit, so it catches that first
    /// transition and nothing after it — `head` is the field that moves on
    /// every commit.
    pub dirty: bool,
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
    /// The checkout this container was built from — see [`ContainerSource`].
    /// `config_hash` above decides *whether* this node drifted; this is what
    /// lets the answer name a commit instead of only a verdict.
    #[serde(default)]
    pub source: Option<ContainerSource>,
    /// Whether this container's desired terminal state is "exited 0" rather
    /// than "running" — true for exactly the nodes resolved with
    /// `kind: "task"` (see `resolver::visit_dependency::visit_task_dependency`).
    ///
    /// The one bit of the node's *kind* that has to travel into runtime
    /// state, because `observed.status == "exited"` means opposite things
    /// for the two: drift for a service, success for a task. Nothing
    /// downstream of the reducer — the drift reconciler, the UI's status
    /// badge, `ensure_running`'s "is it still alive" check — holds the
    /// resolved graph, so without this they would all have to guess.
    ///
    /// Note `running` stays `false` for a task even while its container is
    /// alive: `running` records what fghj wants to be true *at rest*, and
    /// what fghj wants for a task at rest is for it to be finished.
    #[serde(default)]
    pub terminating: bool,
    /// Whether this container was started with `FGHJ_DEBUG_WAIT=1` — the
    /// per-container switch that asks the image to halt at startup until a
    /// debugger attaches.
    ///
    /// The only environment variable fghj sets for debugging at all —
    /// `#RunOptions.debug` itself injects nothing, since an image already
    /// knows which port it listens on. This is the one fact it cannot know:
    /// whether a human has asked *this* container to wait. See
    /// `guides/debugging.md` for what an image does with it.
    ///
    /// Runtime state, not config: it is flipped from the UI per container,
    /// never declared in `.fghj.yaml`, because that file is committed and
    /// shared — pinning it there would halt every teammate's start of this
    /// node indefinitely.
    ///
    /// Deliberately **not** part of `runs::spec::spec_hash`. The hash is a
    /// statement about the node's resolved *config*, and this isn't one; if
    /// it were included, `ensure_running`'s drift check would read a halted
    /// container as drifted and recreate it, killing the debug session the
    /// switch had just established. The cost of that choice is that a
    /// recreate for genuine config drift silently drops the flag — which is
    /// why this is recorded here, on the container, rather than held
    /// separately: the switch shown in the UI is then always what the
    /// running container actually has.
    #[serde(default)]
    pub debug_wait: bool,
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
    /// The exit code Docker reports for a container that has finished —
    /// `None` while it is still running, and `None` for a container fghj
    /// has not re-inspected since it exited. Only meaningful alongside
    /// `ContainerDesired::terminating`: for a service an exit code is just
    /// one more detail of a crash, but for a task it *is* the outcome, and
    /// `Some(0)` versus `Some(n)` is the whole difference between a
    /// migration that ran and one that has to block everything downstream
    /// of it.
    #[serde(default)]
    pub exit_code: Option<i64>,
}

/// One container's full state: what fghj wants (`desired`), what Docker
/// last reported (`observed`), and what's currently in flight
/// (`pending_action`). The single representation — the reducer, the
/// `RunRegistry` that drives Docker, SQLite, and the `/runs` JSON the UI
/// reads all use this one type, so there is nothing to translate between
/// and nothing that can be lost in translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub node_id: String,
    pub desired: ContainerDesired,
    pub observed: ContainerObserved,
    pub pending_action: Option<PendingAction>,
}

/// What a container's `desired`/`observed` pair *means*, in one word.
///
/// Derived, never stored — see `ContainerInfo::condition`. The pair itself is
/// the state; this is the reading of it, kept in one place so the UI, the CLI
/// and anything added later cannot each invent their own slightly different
/// version of the rule.
///
/// The variant that justifies the enum is `Crashed`. `desired.running == true`
/// alongside an `exited` (or `dead`, or `removed`) observation is reachable —
/// the process crashed, or someone ran `docker rm` — and **nothing in fghj
/// converges it**. `DockerConvergeEffect::extract` reads only `pending_action`
/// and `pending_create`, never `observed`; the reconciler is read-only by
/// design ([[run-lifecycle-and-registry]] argues that at length). Only an
/// explicit Start clears it.
///
/// That design is right, but it only works if the user is *told*, and for a
/// long time this pair rendered identically to a container the user had
/// deliberately stopped — the same missing-vocabulary problem
/// `SyncStatus::Orphaned` was added to fix (`concepts/AUDIT.md` B5, B6).
///
/// `Restarting` and `Paused` are here for a smaller version of the same
/// thing: both are real Docker states, both mean the container is not
/// serving — every route lookup filters on `status == "running"`
/// (`state::query`, `effects::hosts`) — and before this both read simply as
/// "stopped", which is not what is happening in either case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeCondition {
    /// Docker reports it up.
    Running,
    /// Docker reports it down, and fghj was not asked for it to be up. The
    /// expected, uninteresting state after a Stop.
    Stopped,
    /// fghj was asked for this to be up and Docker says it is not. Nothing
    /// will fix this on its own.
    Crashed,
    /// Docker is bouncing it under the `restart` policy the config asked
    /// for. Not serving, but not stuck either — worth waiting out.
    Restarting,
    /// `docker pause`, from outside fghj. Not serving; a plain Start will
    /// not help, since the container is not stopped.
    Paused,
    /// A terminating node that exited zero — for a task this is success, not
    /// drift. See `ContainerDesired::terminating`.
    Completed,
    /// A terminating node that exited non-zero. Everything downstream of it
    /// was not started.
    Failed,
    /// A terminating node that has exited, but which fghj has not
    /// re-inspected since, so there is no exit code to read yet. Says "no
    /// verdict", not "success".
    Finishing,
}

impl ContainerInfo {
    /// The one reading of the `desired`/`observed` pair — see
    /// [`NodeCondition`].
    ///
    /// Deliberately says nothing about `pending_action`. That is the
    /// *transient* half of a node's lifecycle and every consumer already
    /// layers it on top ("stopping…" wins over whatever the settled pair
    /// says); folding it in here would make one field answer two different
    /// questions and lose the settled reading while an action is in flight.
    pub fn condition(&self) -> NodeCondition {
        let status = self.observed.status.as_str();
        // A task is read on a different scale: `exited` is failure for a
        // service and the *goal* for a task, and the exit code is the only
        // thing that separates the two ends of that scale.
        if self.desired.terminating {
            return match (status, self.observed.exit_code) {
                ("running", _) => NodeCondition::Running,
                (_, Some(0)) => NodeCondition::Completed,
                (_, Some(_)) => NodeCondition::Failed,
                (_, None) => NodeCondition::Finishing,
            };
        }
        match status {
            "running" => NodeCondition::Running,
            "restarting" => NodeCondition::Restarting,
            "paused" => NodeCondition::Paused,
            // Every remaining Docker status (`exited`, `dead`, `created`,
            // fghj's own `removed`) means "not up". Whether that is fine or
            // broken is not a fact about Docker's word at all — it is
            // whether fghj was asked for this container to be up.
            _ if self.desired.running => NodeCondition::Crashed,
            _ => NodeCondition::Stopped,
        }
    }
}

/// Hand-written rather than derived for one reason: `condition` is a derived
/// field. It has to reach the UI (it is the whole point of
/// [`NodeCondition`]), and it must not become a *stored* field that a reducer
/// arm could forget to update — a cached reading of two fields right next to
/// it is a bug waiting for the one code path that sets `status` without it.
///
/// Everything else is exactly what `#[derive(Serialize)]` produced, including
/// `pending_action` being omitted when `None`.
impl Serialize for ContainerInfo {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let fields = 4 + usize::from(self.pending_action.is_some());
        let mut s = serializer.serialize_struct("ContainerInfo", fields)?;
        s.serialize_field("node_id", &self.node_id)?;
        s.serialize_field("desired", &self.desired)?;
        s.serialize_field("observed", &self.observed)?;
        s.serialize_field("condition", &self.condition())?;
        if let Some(pending) = &self.pending_action {
            s.serialize_field("pending_action", pending)?;
        }
        s.end()
    }
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
            source: None,
            terminating: false,
            debug_wait: false,
        }
    }

    fn info(status: &str, running: bool) -> ContainerInfo {
        ContainerInfo {
            node_id: "web".into(),
            desired: ContainerDesired {
                running,
                ..desired()
            },
            observed: ContainerObserved {
                status: status.into(),
                ..Default::default()
            },
            pending_action: None,
        }
    }

    fn task(status: &str, exit_code: Option<i64>) -> ContainerInfo {
        ContainerInfo {
            node_id: "migrate".into(),
            desired: ContainerDesired {
                running: false,
                terminating: true,
                ..desired()
            },
            observed: ContainerObserved {
                status: status.into(),
                exit_code,
                ..Default::default()
            },
            pending_action: None,
        }
    }

    /// The pair [B6](AUDIT) is about: asked to be up, observed down, and
    /// nothing in fghj will act on it. It must not read the same as a
    /// container the user deliberately stopped.
    #[test]
    fn asked_to_be_up_and_observed_down_is_crashed_not_stopped() {
        assert_eq!(info("exited", true).condition(), NodeCondition::Crashed);
        assert_eq!(info("exited", false).condition(), NodeCondition::Stopped);
    }

    /// Whether "not up" is fine or broken is not a fact about Docker's word
    /// for it — every one of these means the same thing once you know
    /// whether fghj asked for the container to be up.
    #[test]
    fn every_not_up_status_reads_the_same_way() {
        for status in ["exited", "dead", "removed", "created"] {
            assert_eq!(
                info(status, true).condition(),
                NodeCondition::Crashed,
                "{status} while desired-running"
            );
            assert_eq!(
                info(status, false).condition(),
                NodeCondition::Stopped,
                "{status} while not desired-running"
            );
        }
    }

    /// Both are "not serving" — every route lookup filters on `running` —
    /// but neither is stuck, and a Start would not help a paused container.
    #[test]
    fn restarting_and_paused_are_neither_running_nor_stopped() {
        assert_eq!(
            info("restarting", true).condition(),
            NodeCondition::Restarting
        );
        assert_eq!(info("paused", true).condition(), NodeCondition::Paused);
    }

    #[test]
    fn a_task_is_read_on_its_exit_code_not_on_being_down() {
        assert_eq!(
            task("exited", Some(0)).condition(),
            NodeCondition::Completed
        );
        assert_eq!(task("exited", Some(1)).condition(), NodeCondition::Failed);
        // Exited, but not re-inspected since — "no verdict yet", which is
        // emphatically not the same as success.
        assert_eq!(task("exited", None).condition(), NodeCondition::Finishing);
        assert_eq!(task("running", None).condition(), NodeCondition::Running);
    }

    /// The reason `condition` is derived rather than stored: it must reach
    /// the UI, and a cached reading of two adjacent fields is a bug waiting
    /// for the one code path that updates `status` without it.
    #[test]
    fn condition_is_serialized_alongside_the_pair_it_reads() {
        let json = serde_json::to_value(info("exited", true)).unwrap();
        assert_eq!(json["condition"], "crashed");
        assert_eq!(json["observed"]["status"], "exited");
        assert_eq!(json["desired"]["running"], true);
        // Unchanged from the derived impl: omitted when there is none.
        assert!(json.get("pending_action").is_none());
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
