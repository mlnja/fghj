//! [`WorkspaceRegistry`] — the set of wired workspaces, the actor handle
//! for each, and the route/zone projections the proxy and DNS read.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::daemon::workspace_id::workspace_id;
use crate::server::WorkspaceState;
use crate::{actor, daemon_log, effects, persistence, registry, state};

/// The segment a workspace at `path` contributes to every domain its nodes
/// derive — `resolve_universe` takes the canonical path's `file_name` as the
/// workspace name and `runs::derive_domain` runs it through `sanitize_label`.
/// Duplicated here as one small function rather than by calling into the
/// resolver, because this needs the answer for a workspace that has not been
/// resolved yet (and must not be, if it is about to be rejected).
fn domain_label(path: &std::path::Path) -> String {
    crate::util::label::sanitize_label(
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace"),
    )
}

/// In-memory registry of workspaces the daemon knows about, keyed by id.
/// Holds only data (path + per-workspace registries) — there is no thread or
/// listener tied to a workspace; all of them are served off the single axum
/// router, shared via `Router::with_state`.
pub struct WorkspaceRegistry {
    by_id: Mutex<HashMap<String, Arc<WorkspaceState>>>,
    index_path: PathBuf,
    docker: Arc<bollard::Docker>,
    /// New-system actor for every workspace this registry knows about — the
    /// redux-style migration's (rosy-soaring-teapot.md) canonical
    /// `state::WorkspaceState`, authored entirely by the reducer and
    /// converged to real Docker/DNS/hosts/raw-net state by the effects in
    /// `docker_converge_tasks` and `effects::spawn_all`. Kept alongside
    /// `by_id` rather than merged into it: `by_id`'s `server::WorkspaceState`
    /// still owns the Docker-facing machinery (`RunRegistry`, the Docker
    /// client, the database handle) that carries these actions out. It no
    /// longer owns any *state* — that all lives behind these handles now —
    /// so the two are expected to merge.
    actors: registry::ActorRegistry,
    /// Per-workspace `effects::docker::DockerConvergeEffect` driver tasks —
    /// the only thing that ever mutates a workspace's real containers now
    /// that `effects::bridge` (a purely-mirroring, never-mutating stand-in)
    /// is gone. Torn down in `stop`.
    docker_converge_tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl WorkspaceRegistry {
    /// Rebuilds the registry from the central workspace index at the real,
    /// root-owned path. `load_from` does the actual work — split out so
    /// tests can point the index at a tempdir instead.
    pub async fn load(docker: Arc<bollard::Docker>) -> Self {
        Self::load_from(persistence::default_index_path(), docker).await
    }

    pub(crate) async fn load_from(index_path: PathBuf, docker: Arc<bollard::Docker>) -> Self {
        let mut by_id = HashMap::new();
        for (id, path) in persistence::load_index(&index_path) {
            if !path.exists() {
                daemon_log::warn(format!(
                    "fghjd: skipping missing workspace {id} ({})",
                    path.display()
                ));
                continue;
            }
            match WorkspaceState::new(path.clone(), docker.clone()).await {
                Ok(state) => {
                    by_id.insert(id, Arc::new(state));
                }
                Err(e) => daemon_log::warn(format!(
                    "fghjd: failed to load workspace {id} ({}): {e}",
                    path.display()
                )),
            }
        }
        let registry = Self {
            by_id: Mutex::new(by_id),
            index_path,
            docker,
            actors: registry::ActorRegistry::new(),
            docker_converge_tasks: Mutex::new(HashMap::new()),
        };
        let loaded: Vec<(String, Arc<WorkspaceState>)> = registry
            .by_id
            .lock()
            .unwrap()
            .iter()
            .map(|(id, state)| (id.clone(), state.clone()))
            .collect();
        for (id, state) in loaded {
            registry.wire_actor(&id, &state);
        }
        registry
    }

    /// Spawns the actor for `id`, seeded with a one-shot snapshot of
    /// `old.rehydrated` — what a previous `fghjd` lifetime left running,
    /// reconciled against real Docker once when the workspace was
    /// constructed — so a freshly-wired workspace with pre-existing runs
    /// doesn't start out looking empty, then starts the
    /// `DockerConvergeEffect` task that's the only thing driving real
    /// Docker calls from here on. `effects::bridge`, which used to keep
    /// this state live by re-polling a second copy of it in
    /// `RunRegistry`, is gone as of migration phase 5, and so is that
    /// second copy: from here the actor is the only store.
    ///
    /// Called from both
    /// `load_from` (startup) and `resolve` (a fresh `fghj ui`/wire) — every
    /// workspace this registry ever registers also gets a wired actor,
    /// right at the exact place its `Arc<WorkspaceState>` is born.
    fn wire_actor(&self, id: &str, old: &Arc<WorkspaceState>) {
        let seed = state::WorkspaceState {
            runs: old.rehydrated.clone(),
            ..Default::default()
        };
        let handle = actor::spawn(seed);
        let docker_converge_task = tokio::spawn(effects::run_effect(
            effects::docker::DockerConvergeEffect::new(old.clone(), handle.clone()),
            handle.subscribe(),
            "docker_converge",
        ));
        // Persistence and the sidecar route table are both pure functions
        // of published state, so they are derived here rather than written
        // by hand at the end of every lifecycle call in `runs/`. See
        // `effects::persist` and `effects::routes` for why that move fixes
        // a real class of stale write, not just tidiness.
        tokio::spawn(effects::run_async_effect(
            effects::persist::PersistEffect::new(old.db.clone()),
            handle.subscribe(),
            "persist",
        ));
        tokio::spawn(effects::run_effect(
            effects::routes::RouteTableEffect::new(),
            handle.subscribe(),
            "route_table",
        ));
        self.actors
            .insert(id.to_string(), registry::WorkspaceHandle { actor: handle });
        self.docker_converge_tasks
            .lock()
            .unwrap()
            .insert(id.to_string(), docker_converge_task);
    }

    /// The daemon-wide directory of new-system actors this registry has
    /// wired — used by `DaemonControl::activate` to subscribe the raw-net
    /// fanned-in effect.
    pub fn actors(&self) -> &registry::ActorRegistry {
        &self.actors
    }

    /// Resolves (cloning `entry` if needed) and registers a workspace,
    /// reusing the existing entry if this path is already known. Errors if
    /// the path is nested inside an already-wired workspace — a workspace
    /// root covers its whole subtree, so a second registration underneath it
    /// would just be an alias for part of the same tree — or if its folder
    /// name collides with another workspace's in the domain namespace (see
    /// [`domain_label`]).
    pub async fn resolve(
        &self,
        entry: Option<String>,
        workspace: Option<PathBuf>,
        owner: Option<persistence::WorkspaceOwner>,
    ) -> Result<(String, PathBuf)> {
        let entry_for_meta = entry.clone();
        let owner_for_clone = owner.clone();
        let path = tokio::task::spawn_blocking(move || {
            crate::resolve_workspace(entry, workspace, owner_for_clone.as_ref())
        })
        .await
        .context("resolve_workspace task panicked")??;
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);

        {
            let by_id = self.by_id.lock().unwrap();
            for state in by_id.values() {
                if canonical != state.path && canonical.starts_with(&state.path) {
                    bail!(
                        "{} is inside the already-wired workspace {}",
                        canonical.display(),
                        state.path.display()
                    );
                }
                // Workspaces are keyed by *id* here but by *name* in every
                // derived domain, and `resolve_route` scans all of them. Two
                // workspaces whose folder names sanitize alike — `~/work/shop`
                // and `~/scratch/shop`, which review runs make a normal thing
                // to want — produce byte-identical domains for their nodes,
                // and traffic goes to whichever the `BTreeMap` yields first.
                // There is no way to disambiguate after the fact, so the only
                // place to catch it is here, before the second one exists.
                if canonical != state.path && domain_label(&canonical) == domain_label(&state.path)
                {
                    bail!(
                        "{} and the already-wired workspace {} both derive the \
                         domain segment '{}', so their services would claim \
                         identical *.fghj.internal names. Rename one folder.",
                        canonical.display(),
                        state.path.display(),
                        domain_label(&canonical),
                    );
                }
            }
        }

        let id = workspace_id(&canonical);
        let existing = self.by_id.lock().unwrap().get(&id).cloned();
        let state = match existing {
            Some(state) => state,
            None => {
                let state =
                    Arc::new(WorkspaceState::new(canonical.clone(), self.docker.clone()).await?);
                state
                    .db
                    .clone()
                    .record_meta(id.clone(), entry_for_meta)
                    .await?;
                self.by_id.lock().unwrap().insert(id.clone(), state.clone());
                self.wire_actor(&id, &state);

                let mut index = persistence::load_index(&self.index_path);
                index.insert(id.clone(), canonical.clone());
                persistence::save_index(&self.index_path, &index)?;
                state
            }
        };

        // Refreshed on every `wire`, not just the first — the ssh-agent
        // socket captured here is only valid for the CLI's current login
        // session, so a later `wire` from a fresh session should replace it.
        if let Some(owner) = owner {
            state.db.clone().set_owner(id.clone(), owner).await?;
        }

        Ok((id, canonical))
    }

    pub fn get(&self, id: &str) -> Option<Arc<WorkspaceState>> {
        self.by_id.lock().unwrap().get(id).cloned()
    }

    /// Every wired workspace's reducer-owned state, as of right now — the
    /// input to the `state::query` projections that back live HTTPS routing
    /// (`daemon::routing`), DNS answering (ditto), and the network
    /// telemetry endpoint. A live read path deliberately goes through the
    /// same actor state the converge effects do, so what the proxy routes
    /// and what `pf`/`/etc/resolver` were configured for can't disagree.
    pub fn states(&self) -> BTreeMap<String, Arc<state::WorkspaceState>> {
        self.actors.states()
    }

    pub fn list(&self) -> Vec<(String, PathBuf)> {
        self.by_id
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| (id.clone(), s.path.clone()))
            .collect()
    }

    /// Stops every live run in the workspace, forgets it in memory, and
    /// drops it from the central index so a future `fghjd` restart doesn't
    /// bring it back.
    pub async fn stop(&self, id: &str) -> bool {
        let removed = self.by_id.lock().unwrap().remove(id);
        match removed {
            Some(state) => {
                // Read the run states *before* deregistering: the actor's
                // published `WorkspaceState` is the only record of what is
                // running, so dropping the handle first would leave nothing
                // to tear down and strand the containers.
                let runs = self
                    .actors
                    .get(id)
                    .map(|handle| handle.actor.current().runs.clone())
                    .unwrap_or_default();
                self.actors.remove(id);
                if let Some(task) = self.docker_converge_tasks.lock().unwrap().remove(id) {
                    task.abort();
                }
                for run in runs.values() {
                    let _ = state.runs.stop(&run.run_id, run).await;
                }
                let mut index = persistence::load_index(&self.index_path);
                index.remove(id);
                let _ = persistence::save_index(&self.index_path, &index);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::daemon::bootstrap::connect_docker;

    pub(crate) fn test_docker() -> Arc<bollard::Docker> {
        Arc::new(connect_docker().expect("docker client construction"))
    }

    #[test]
    fn domain_label_matches_what_derive_domain_would_produce() {
        // Same folder name, two parents — the whole point is that the parent
        // contributes nothing, which is exactly why these collide.
        assert_eq!(
            domain_label(std::path::Path::new("/home/me/work/shop")),
            domain_label(std::path::Path::new("/home/me/scratch/shop"))
        );
        // And that it really is `sanitize_label`'s output, not the raw name.
        assert_eq!(
            domain_label(std::path::Path::new("/tmp/My Shop")),
            "my-shop"
        );
    }

    /// B4: workspaces are keyed by id but named by folder in every derived
    /// domain, and `resolve_route` scans all of them.
    #[tokio::test]
    async fn resolve_rejects_a_workspace_whose_name_collides_with_a_wired_one() {
        let tmp = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;

        registry
            .resolve(None, Some(tmp.path().join("work/shop")), None)
            .await
            .unwrap();
        let err = registry
            .resolve(None, Some(tmp.path().join("scratch/shop")), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("identical *.fghj.internal names"),
            "unexpected error message: {err}"
        );

        // A differently-named sibling is still fine — the check must not have
        // turned into "one workspace at a time".
        registry
            .resolve(None, Some(tmp.path().join("scratch/other")), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn resolve_is_idempotent_and_rejects_nested_workspaces() {
        let tmp = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;

        let root = tmp.path().join("root");
        let (id1, canonical) = registry
            .resolve(None, Some(root.clone()), None)
            .await
            .unwrap();

        // re-wiring the same path returns the same id, not a duplicate
        let (id2, _) = registry
            .resolve(None, Some(root.clone()), None)
            .await
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(registry.list().len(), 1);

        // the index on disk should reflect the single registered workspace
        let persisted = persistence::load_index(&tmp.path().join("workspaces.json"));
        assert_eq!(persisted.get(&id1), Some(&canonical));

        // registering a path inside an already-wired workspace must error
        let nested = root.join("nested-service");
        let err = registry
            .resolve(None, Some(nested), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("is inside the already-wired workspace"),
            "unexpected error message: {err}"
        );
    }

    #[tokio::test]
    async fn stop_removes_workspace_from_registry_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;
        let (id, _) = registry
            .resolve(None, Some(tmp.path().join("root")), None)
            .await
            .unwrap();

        assert!(registry.stop(&id).await);
        assert!(registry.get(&id).is_none());
        assert!(!persistence::load_index(&tmp.path().join("workspaces.json")).contains_key(&id));
        // stopping an unknown id is reported, not a panic
        assert!(!registry.stop(&id).await);
    }
}
