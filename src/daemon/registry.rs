use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use tokio::sync::watch;

use crate::activity::{Activity, ActivityLog};
use crate::actor::ActorHandle;
use crate::daemon::removal::{self, Removals};
use crate::daemon::workspace_id::workspace_id;
use crate::server::WorkspaceState;
use crate::web::traffic::TrafficStore;
use crate::{actor, daemon_log, effects, persistence, state, supervisor};

/// Mirrors what `resolve_universe` and `derive_domain` produce, for a workspace
/// that hasn't been (and may never be) resolved.
fn domain_label(path: &std::path::Path) -> String {
    crate::util::label::sanitize_label(
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace"),
    )
}

/// Canonicalizes the deepest existing ancestor and re-appends the rest, so a
/// wire can be checked before `resolve_workspace` creates anything. Only used
/// for comparison.
fn canonicalize_lossy(path: &std::path::Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(real) = std::fs::canonicalize(ancestor) {
            return match path.strip_prefix(ancestor) {
                Ok(rest) => real.join(rest),
                Err(_) => real,
            };
        }
    }
    path.to_path_buf()
}

/// Both directions: overlapping workspaces claim the same repos and run two
/// reconcilers over one set of containers. `Path::starts_with` is
/// component-wise, so `shop` and `shop-legacy` don't conflict. Both paths must
/// be canonical.
fn nesting_conflict(candidate: &std::path::Path, existing: &std::path::Path) -> Option<String> {
    if candidate == existing {
        return None;
    }
    if candidate.starts_with(existing) {
        return Some(format!(
            "{} is inside the already-wired workspace {}. Workspaces cannot \
             nest — wire it separately outside that directory, or stop the \
             outer workspace first.",
            candidate.display(),
            existing.display()
        ));
    }
    if existing.starts_with(candidate) {
        return Some(format!(
            "{} contains the already-wired workspace {}. Workspaces cannot \
             nest — wire a directory that doesn't enclose it, or stop the \
             inner workspace first.",
            candidate.display(),
            existing.display()
        ));
    }
    None
}

pub struct WorkspaceRegistry {
    by_id: Mutex<HashMap<String, Arc<WorkspaceState>>>,
    index_path: PathBuf,
    docker: Arc<bollard::Docker>,
    /// A `watch` so the daemon-wide effects notice workspaces being wired or
    /// stopped. Only changed alongside `by_id`.
    actors: watch::Sender<effects::ActorMap>,
    /// The only thing that mutates a workspace's real containers.
    docker_converge_tasks: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// Recorded proxy traffic. In memory only: it is gone when fghjd stops.
    traffic: TrafficStore,
    activities: Arc<ActivityLog>,
    removals: Arc<Removals>,
}

impl WorkspaceRegistry {
    pub async fn load(docker: Arc<bollard::Docker>) -> Self {
        Self::load_from(persistence::default_index_path(), docker).await
    }

    pub(crate) async fn load_from(index_path: PathBuf, docker: Arc<bollard::Docker>) -> Self {
        let activities = Arc::new(ActivityLog::default());
        let removals = Arc::new(Removals::beside(&index_path));
        let pending = removals.pending();
        let mut index = persistence::load_index(&index_path);
        // Marked before it left the index: fghjd stopped in between.
        let stale = pending
            .keys()
            .filter(|id| index.remove(*id).is_some())
            .count();
        if stale > 0
            && let Err(e) = persistence::save_index(&index_path, &index)
        {
            daemon_log::warn(format!("fghjd: {e:#}"));
        }
        let mut by_id = HashMap::new();
        for (id, path) in index {
            if !path.exists() {
                daemon_log::warn(format!(
                    "fghjd: skipping missing workspace {id} ({})",
                    path.display()
                ));
                continue;
            }
            match WorkspaceState::new(id.clone(), path.clone(), docker.clone(), activities.clone())
                .await
            {
                Ok(state) => {
                    by_id.insert(id, Arc::new(state));
                }
                Err(e) => daemon_log::warn(format!(
                    "fghjd: failed to load workspace {id} ({}): {e}",
                    path.display()
                )),
            }
        }
        // An old index can still hold a nested pair. Warned, not dropped: de-wiring a
        // workspace with live runs is worse. The CLI picks the innermost.
        let loaded_paths: Vec<(String, PathBuf)> = by_id
            .iter()
            .map(|(id, state)| (id.clone(), state.path.clone()))
            .collect();
        for (i, (id_a, path_a)) in loaded_paths.iter().enumerate() {
            for (id_b, path_b) in loaded_paths.iter().skip(i + 1) {
                if nesting_conflict(path_a, path_b).is_some() {
                    daemon_log::warn(format!(
                        "fghjd: wired workspaces {id_a} ({}) and {id_b} ({}) overlap — one \
                         contains the other. They scan the same repos and will fight over the \
                         same containers; stop one of them. Newer fghjd refuses to wire this.",
                        path_a.display(),
                        path_b.display()
                    ));
                }
            }
        }

        let registry = Self {
            by_id: Mutex::new(by_id),
            index_path,
            docker,
            actors: watch::Sender::new(Arc::new(BTreeMap::new())),
            docker_converge_tasks: Mutex::new(HashMap::new()),
            traffic: TrafficStore::default(),
            activities,
            removals,
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
        for (id, path) in pending {
            daemon_log::warn(format!(
                "fghjd: finishing the removal of workspace {id} ({})",
                path.display()
            ));
            registry.spawn_teardown(&id, path);
        }
        registry
    }

    /// Seeds the actor from `old.rehydrated`, so a workspace with a running
    /// environment doesn't start out empty.
    fn wire_actor(&self, id: &str, old: &Arc<WorkspaceState>) {
        let seed = state::WorkspaceState {
            env: old.rehydrated.clone(),
            ..Default::default()
        };
        let handle = actor::spawn(seed);
        let docker_converge_task = supervisor::supervise(
            format!("docker_converge[{id}]"),
            effects::run_effect(
                effects::docker::DockerConvergeEffect::new(old.clone(), handle.clone()),
                handle.subscribe(),
                "docker_converge",
            ),
        );
        supervisor::supervise(
            format!("persist[{id}]"),
            effects::run_async_effect(
                effects::persist::PersistEffect::new(old.db.clone()),
                handle.subscribe(),
                "persist",
            ),
        );
        supervisor::supervise(
            format!("route_table[{id}]"),
            effects::run_effect(
                effects::routes::RouteTableEffect::new(),
                handle.subscribe(),
                "route_table",
            ),
        );
        self.actors.send_modify(|actors| {
            Arc::make_mut(actors).insert(id.to_string(), handle);
        });
        self.docker_converge_tasks
            .lock()
            .unwrap()
            .insert(id.to_string(), docker_converge_task);
    }

    pub fn actor(&self, id: &str) -> Option<ActorHandle> {
        self.actors.borrow().get(id).cloned()
    }

    /// Fresh per caller, like `ActorHandle::subscribe`.
    pub fn subscribe_actors(&self) -> watch::Receiver<effects::ActorMap> {
        self.actors.subscribe()
    }

    /// The nesting check runs twice: before `resolve_workspace` so a rejected wire
    /// leaves no clone behind, and after it on the truly canonical path.
    pub async fn resolve(
        &self,
        entry: Option<String>,
        workspace: Option<PathBuf>,
        owner: Option<persistence::WorkspaceOwner>,
    ) -> Result<(String, PathBuf)> {
        if let Some(canonical) = workspace.as_deref().map(canonicalize_lossy) {
            let conflict = {
                let by_id = self.by_id.lock().unwrap();
                by_id
                    .values()
                    .find_map(|state| nesting_conflict(&canonical, &state.path))
            };
            if let Some(conflict) = conflict {
                bail!(conflict);
            }
        }

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
                if let Some(conflict) = nesting_conflict(&canonical, &state.path) {
                    bail!(conflict);
                }
                // Workspaces whose folder names sanitize alike derive identical domains, and
                // `resolve_route` would pick one arbitrarily.
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
        // Its Docker names would be this one's, so its teardown would take
        // this one's containers too.
        if let Some(path) = self
            .removals
            .pending()
            .into_values()
            .find(|path| domain_label(path) == domain_label(&canonical))
        {
            bail!(
                "{} is still being removed, and {} would get the same \
                 container and network names. Wire it again once that's done.",
                path.display(),
                canonical.display(),
            );
        }

        let id = workspace_id(&canonical);
        let existing = self.by_id.lock().unwrap().get(&id).cloned();
        let state = match existing {
            Some(state) => state,
            None => {
                let state = Arc::new(
                    WorkspaceState::new(
                        id.clone(),
                        canonical.clone(),
                        self.docker.clone(),
                        self.activities.clone(),
                    )
                    .await?,
                );
                self.by_id.lock().unwrap().insert(id.clone(), state.clone());
                self.wire_actor(&id, &state);

                let mut index = persistence::load_index(&self.index_path);
                index.insert(id.clone(), canonical.clone());
                persistence::save_index(&self.index_path, &index)?;
                state
            }
        };

        // On every wire: the ssh-agent socket is only valid for the CLI's session.
        if let Some(owner) = owner {
            owner.take_workspace(&canonical)?;
            state.db.clone().set_owner(owner).await?;
        }

        Ok((id, canonical))
    }

    pub fn traffic(&self) -> &TrafficStore {
        &self.traffic
    }

    pub fn activities(&self) -> &ActivityLog {
        &self.activities
    }

    pub fn get(&self, id: &str) -> Option<Arc<WorkspaceState>> {
        self.by_id.lock().unwrap().get(id).cloned()
    }

    /// Live reads go through the same actor state the effects converge from, so the
    /// proxy, the raw-zone listeners and the OS resolver can't disagree.
    pub fn states(&self) -> BTreeMap<String, Arc<state::WorkspaceState>> {
        self.actors
            .borrow()
            .iter()
            .map(|(id, actor)| (id.clone(), actor.current()))
            .collect()
    }

    /// For `/daemon/doctor`: a fresh client would test whatever `connect_docker`
    /// finds now, not what the workspaces use.
    pub(crate) fn docker(&self) -> Arc<bollard::Docker> {
        Arc::clone(&self.docker)
    }

    pub fn list(&self) -> Vec<(String, PathBuf)> {
        self.by_id
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s)| (id.clone(), s.path.clone()))
            .collect()
    }

    /// Takes `id` off the registry and the index now and starts tearing its
    /// containers and network down in the background. `None` if `id` is
    /// neither wired nor being removed. A removal that failed is retried.
    pub fn remove(&self, id: &str) -> Result<Option<Activity>> {
        let Some(state) = self.get(id) else {
            return Ok(self
                .removals
                .pending()
                .remove(id)
                .map(|path| self.spawn_teardown(id, path)));
        };
        // First, so a crash past this point finishes the removal on start.
        self.removals.add(id, &state.path)?;

        self.by_id.lock().unwrap().remove(id);
        self.actors.send_modify(|actors| {
            Arc::make_mut(actors).remove(id);
        });
        if let Some(task) = self.docker_converge_tasks.lock().unwrap().remove(id) {
            task.abort();
        }
        let mut index = persistence::load_index(&self.index_path);
        index.remove(id);
        if let Err(e) = persistence::save_index(&self.index_path, &index) {
            // Not fatal: `load_from` drops what `removing.json` lists.
            daemon_log::warn(format!("fghjd: {e:#}"));
        }
        Ok(Some(self.spawn_teardown(id, state.path.clone())))
    }

    fn spawn_teardown(&self, id: &str, path: PathBuf) -> Activity {
        removal::spawn_teardown(
            self.docker.clone(),
            &self.activities,
            self.removals.clone(),
            id,
            path,
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use super::*;

    /// Never dialled; the converge effect fails its first request and goes quiet.
    pub(crate) fn test_docker() -> Arc<bollard::Docker> {
        Arc::new(crate::docker::undialled_client())
    }

    #[test]
    fn domain_label_matches_what_derive_domain_would_produce() {
        assert_eq!(
            domain_label(std::path::Path::new("/home/me/work/shop")),
            domain_label(std::path::Path::new("/home/me/scratch/shop"))
        );
        assert_eq!(
            domain_label(std::path::Path::new("/tmp/My Shop")),
            "my-shop"
        );
    }

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

        registry
            .resolve(None, Some(tmp.path().join("scratch/other")), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rejecting_a_nested_workspace_does_not_create_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;

        let root = tmp.path().join("root");
        registry
            .resolve(None, Some(root.clone()), None)
            .await
            .unwrap();

        let nested = root.join("not-yet-there");
        assert!(
            registry
                .resolve(None, Some(nested.clone()), None)
                .await
                .is_err()
        );
        assert!(
            !nested.exists(),
            "{} was created by a wire that was rejected",
            nested.display()
        );
    }

    #[test]
    fn canonicalize_lossy_resolves_the_existing_part_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(
            canonicalize_lossy(&tmp.path().join("a/b/c")),
            real.join("a/b/c")
        );
    }

    #[test]
    fn nesting_conflict_rejects_a_candidate_inside_an_existing_workspace() {
        let conflict = nesting_conflict(Path::new("/w/shop/api"), Path::new("/w/shop"));
        assert!(
            conflict
                .as_deref()
                .is_some_and(|c| c.contains("is inside the already-wired workspace")),
            "unexpected: {conflict:?}"
        );
    }

    #[test]
    fn nesting_conflict_rejects_a_candidate_containing_an_existing_workspace() {
        let conflict = nesting_conflict(Path::new("/w"), Path::new("/w/shop"));
        assert!(
            conflict
                .as_deref()
                .is_some_and(|c| c.contains("contains the already-wired workspace")),
            "unexpected: {conflict:?}"
        );
    }

    #[test]
    fn nesting_conflict_allows_rewiring_the_identical_path() {
        assert!(nesting_conflict(Path::new("/w/shop"), Path::new("/w/shop")).is_none());
    }

    #[test]
    fn nesting_conflict_allows_a_sibling_sharing_a_name_prefix() {
        assert!(nesting_conflict(Path::new("/w/shop-legacy"), Path::new("/w/shop")).is_none());
        assert!(nesting_conflict(Path::new("/w/shop"), Path::new("/w/shop-legacy")).is_none());
    }

    #[test]
    fn nesting_conflict_allows_unrelated_trees() {
        assert!(nesting_conflict(Path::new("/a/shop"), Path::new("/b/shop")).is_none());
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

        let (id2, _) = registry
            .resolve(None, Some(root.clone()), None)
            .await
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(registry.list().len(), 1);

        let persisted = persistence::load_index(&tmp.path().join("workspaces.json"));
        assert_eq!(persisted.get(&id1), Some(&canonical));

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

        let err = registry
            .resolve(None, Some(tmp.path().to_path_buf()), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("contains the already-wired workspace"),
            "unexpected error message: {err}"
        );

        assert_eq!(registry.list().len(), 1);
    }

    #[tokio::test]
    async fn remove_takes_the_workspace_off_at_once_and_marks_it_until_torn_down() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("workspaces.json");
        let registry = WorkspaceRegistry::load_from(index_path.clone(), test_docker()).await;
        let (id, _) = registry
            .resolve(None, Some(tmp.path().join("root")), None)
            .await
            .unwrap();

        let job = registry.remove(&id).unwrap().expect("a removal job");
        assert_eq!(job.workspace_id.as_deref(), Some(id.as_str()));
        assert!(registry.get(&id).is_none());
        assert!(!persistence::load_index(&index_path).contains_key(&id));
        // The undialled Docker fails the teardown, so the mark stays.
        assert!(registry.removals.pending().contains_key(&id));
        assert!(registry.remove("no-such-workspace").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_removal_interrupted_before_leaving_the_index_is_not_loaded_again() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("workspaces.json");
        let root = tmp.path().join("root");
        let id = {
            let registry = WorkspaceRegistry::load_from(index_path.clone(), test_docker()).await;
            let (id, path) = registry.resolve(None, Some(root), None).await.unwrap();
            registry.removals.add(&id, &path).unwrap();
            id
        };

        let registry = WorkspaceRegistry::load_from(index_path.clone(), test_docker()).await;
        assert!(registry.get(&id).is_none());
        assert!(!persistence::load_index(&index_path).contains_key(&id));
        let job = registry
            .activities()
            .get(
                Some(&id),
                crate::activity::ActivityKind::Remove,
                removal::ACTIVITY_KEY,
            )
            .expect("the removal resumed");
        assert_eq!(job.kind, crate::activity::ActivityKind::Remove);
    }

    #[tokio::test]
    async fn a_folder_named_like_one_being_removed_cannot_be_wired() {
        let tmp = tempfile::tempdir().unwrap();
        let registry =
            WorkspaceRegistry::load_from(tmp.path().join("workspaces.json"), test_docker()).await;
        registry
            .removals
            .add("old", &tmp.path().join("a").join("shop"))
            .unwrap();
        let err = registry
            .resolve(None, Some(tmp.path().join("b").join("shop")), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is still being removed"), "{err}");
    }
}
