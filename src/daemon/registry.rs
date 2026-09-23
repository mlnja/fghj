//! [`WorkspaceRegistry`] — the set of wired workspaces, the actor handle
//! for each, and the route/zone projections the proxy and DNS read.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use crate::daemon::workspace_id::workspace_id;
use crate::server::WorkspaceState;
use crate::{actor, daemon_log, effects, persistence, registry, state};

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
    /// `by_id` rather than merged into it: `by_id`'s old `server::WorkspaceState`
    /// still owns the real Docker orchestration (`RunRegistry`) this phase
    /// reuses, persistence, and the live routing/DNS lookup paths that
    /// haven't migrated yet.
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
    /// `old`'s live `runs::RunRegistry` so a freshly-wired workspace with
    /// pre-existing/persisted runs doesn't start out looking empty, then
    /// starts the `DockerConvergeEffect` task that's the only thing driving
    /// real Docker calls from here on — `effects::bridge`, which used to
    /// keep this state live by re-polling `RunRegistry::list()` roughly
    /// once a second, is gone as of migration phase 5.
    ///
    /// The seed is a plain re-keying, not a conversion: `RunRegistry` holds
    /// the same `state::RunState` the actor does. Called from both
    /// `load_from` (startup) and `resolve` (a fresh `fghj ui`/wire) — every
    /// workspace this registry ever registers also gets a wired actor,
    /// right at the exact place its `Arc<WorkspaceState>` is born.
    fn wire_actor(&self, id: &str, old: &Arc<WorkspaceState>) {
        let seed = state::WorkspaceState {
            runs: old
                .runs
                .list()
                .into_iter()
                .map(|run| (run.run_id.clone(), run))
                .collect(),
            ..Default::default()
        };
        let handle = actor::spawn(seed);
        let docker_converge_task = tokio::spawn(effects::run_effect(
            effects::docker::DockerConvergeEffect::new(old.clone(), handle.clone()),
            handle.subscribe(),
            "docker_converge",
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
    /// would just be an alias for part of the same tree.
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
                self.actors.remove(id);
                if let Some(task) = self.docker_converge_tasks.lock().unwrap().remove(id) {
                    task.abort();
                }
                for run in state.runs.list() {
                    let _ = state.runs.stop(&run.run_id).await;
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
