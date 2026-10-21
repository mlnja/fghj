use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use super::git::repo_facts;
use super::graph::{Graph, Node};
use super::repo_url::normalize_repo_url;
use super::visit::ResolveCtx;
use super::workspace_scan::scan_workspace;
use anyhow::Result;

/// Walks the filesystem and shells out to git, so on the blocking pool.
///
/// One resolve per workspace at a time, shared: the UI's polls, the sync
/// reconciler and starts all ask, and overlapping resolves fight over the same
/// repos until none finishes. A caller takes the result of a resolve that began
/// after it asked, never an older one, so a start still sees a `.fghj.yaml`
/// edit made just before it.
pub async fn resolve_universe_async(workspace: PathBuf) -> Result<Graph> {
    let shared = {
        let mut all = SHARED.lock().unwrap();
        all.entry(workspace.clone()).or_default().clone()
    };
    let asked = TICKETS.fetch_add(1, Ordering::SeqCst);
    let _turn = shared.running.lock().await;
    if let Some((began, result)) = shared.last.lock().unwrap().clone()
        && began > asked
    {
        return result.map_err(|e| anyhow::anyhow!(e));
    }
    let began = TICKETS.fetch_add(1, Ordering::SeqCst);
    let result = tokio::task::spawn_blocking(move || resolve_universe(&workspace))
        .await
        .map_err(|e| anyhow::anyhow!("resolve_universe task panicked: {e}"))
        .and_then(|r| r)
        .map_err(|e| format!("{e:#}"));
    *shared.last.lock().unwrap() = Some((began, result.clone()));
    result.map_err(|e| anyhow::anyhow!(e))
}

/// Orders asking against beginning, across workspaces: only order matters.
static TICKETS: AtomicU64 = AtomicU64::new(0);

static SHARED: LazyLock<Mutex<HashMap<PathBuf, Arc<SharedResolve>>>> =
    LazyLock::new(Default::default);

#[derive(Default)]
struct SharedResolve {
    running: tokio::sync::Mutex<()>,
    /// The last resolve's ticket and result. The error as text: `anyhow::Error`
    /// isn't `Clone`.
    last: Mutex<Option<(u64, std::result::Result<Graph, String>)>>,
}

/// An included repo that isn't on disk becomes a `downloaded: false` stub.
pub fn resolve_universe(workspace: &Path) -> Result<Graph> {
    let scanned = scan_workspace(workspace)?;
    let scan_warnings = scanned.warnings;
    let scanned = scanned.components;

    let facts = repo_facts(
        scanned
            .keys()
            .map(|local_path| (local_path.as_str(), workspace.join(local_path))),
    );
    let mut repo_index: HashMap<String, String> = HashMap::new();
    // In folder order, so the first of two clones of one remote wins as before.
    for local_path in scanned.keys() {
        if let Some(repo) = facts.get(local_path).and_then(|f| f.repo.as_ref()) {
            repo_index
                .entry(normalize_repo_url(repo))
                .or_insert_with(|| local_path.clone());
        }
    }

    let mut ctx = ResolveCtx::new(workspace, &scanned, repo_index, facts);
    // Each phase reads what the previous one produced for *every* repo.
    for (local_path, component) in &scanned {
        ctx.add_repo_nodes(local_path, component);
    }
    for (local_path, component) in &scanned {
        ctx.add_includes(local_path, component);
    }
    for (local_path, component) in &scanned {
        ctx.mark_tasks(local_path, component);
    }
    for (local_path, component) in &scanned {
        ctx.add_dependency_edges(local_path, component);
    }
    ctx.add_store_edges();
    ctx.check_references();
    ctx.check_source_paths();
    let flows = ctx.resolve_flows();

    let workspace_name = workspace_name(workspace);

    // Sorted so node order, and the UI layout, is stable across polls.
    let mut nodes: Vec<Node> = ctx.nodes.into_values().collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    for node in &mut nodes {
        node.domain = crate::orchestrator::derive_domain(
            &node.id,
            &workspace_name,
            crate::orchestrator::DomainZone::Http,
        );
    }

    let edges = ctx.edges;

    let mut warnings = scan_warnings;
    warnings.extend(ctx.warnings);
    warnings.extend(super::uniqueness::check_derived_name_collisions(&nodes));
    warnings.extend(super::cycles::check_cycles(&edges));

    Ok(Graph {
        workspace_name,
        nodes,
        edges,
        flows,
        warnings,
    })
}

/// Shared so code finding Docker objects agrees with the code that named them.
pub(crate) fn workspace_name(workspace: &Path) -> String {
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::tests::write_component;

    #[tokio::test(flavor = "multi_thread")]
    async fn callers_asking_at_once_all_get_the_graph() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(tmp.path(), "app", "  image: nginx:alpine\n");

        let calls: Vec<_> = (0..8)
            .map(|_| tokio::spawn(resolve_universe_async(tmp.path().to_path_buf())))
            .collect();
        for call in calls {
            let graph = call.await.unwrap().unwrap();
            assert_eq!(graph.nodes.len(), 1);
        }
    }

    #[tokio::test]
    async fn a_resolve_asked_after_an_edit_sees_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_component(tmp.path(), "app", "  image: nginx:alpine\n");
        let before = resolve_universe_async(tmp.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(before.nodes[0].image.as_deref(), Some("nginx:alpine"));

        write_component(tmp.path(), "app", "  image: redis:7\n");
        let after = resolve_universe_async(tmp.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(after.nodes[0].image.as_deref(), Some("redis:7"));
    }
}
