use std::collections::HashMap;
use std::path::Path;

use super::git::git_remote_and_branch;
use super::graph::{Graph, Node};
use super::repo_url::normalize_repo_url;
use super::visit::ResolveCtx;
use super::workspace_scan::scan_workspace;
use anyhow::Result;

/// Walks the filesystem and shells out to git, so on the blocking pool.
pub async fn resolve_universe_async(workspace: std::path::PathBuf) -> Result<Graph> {
    tokio::task::spawn_blocking(move || resolve_universe(&workspace))
        .await
        .map_err(|e| anyhow::anyhow!("resolve_universe task panicked: {e}"))?
}

/// An included repo that isn't on disk becomes a `downloaded: false` stub.
pub fn resolve_universe(workspace: &Path) -> Result<Graph> {
    let scanned = scan_workspace(workspace)?;
    let scan_warnings = scanned.warnings;
    let scanned = scanned.components;

    let mut repo_index: HashMap<String, String> = HashMap::new();
    for local_path in scanned.keys() {
        let (repo, _branch) = git_remote_and_branch(&workspace.join(local_path));
        if let Some(repo) = repo {
            repo_index
                .entry(normalize_repo_url(&repo))
                .or_insert_with(|| local_path.clone());
        }
    }

    let mut ctx = ResolveCtx::new(workspace, &scanned, repo_index);
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
