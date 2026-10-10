//! `resolve_universe` — the orchestrator that scans the workspace, runs the
//! [`super::visit`] phases over every component, expands flows, then
//! annotates the result with domains.

use std::collections::HashMap;
use std::path::Path;

use super::git::git_remote_and_branch;
use super::graph::{Graph, Node};
use super::repo_url::normalize_repo_url;
use super::visit::ResolveCtx;
use super::workspace_scan::scan_workspace;
use anyhow::Result;

/// [`resolve_universe`] for async callers: it walks the filesystem and
/// shells out to git, so it runs on the blocking pool.
pub async fn resolve_universe_async(workspace: std::path::PathBuf) -> Result<Graph> {
    tokio::task::spawn_blocking(move || resolve_universe(&workspace))
        .await
        .map_err(|e| anyhow::anyhow!("resolve_universe task panicked: {e}"))?
}

/// Resolves every repo currently on disk in the workspace into one shared
/// graph — the "static full universe" the UI lays out once. Any repo can
/// declare `flows`; there is no distinguished "root" repo. An included repo
/// that isn't on disk is rendered as a `downloaded: false` stub instead of
/// blocking resolution — call `pull_all` to clone everything reachable and
/// try again.
pub fn resolve_universe(workspace: &Path) -> Result<Graph> {
    let scanned = scan_workspace(workspace)?;
    // Held aside and folded in at the end alongside every other finding —
    // a repo fghj could not read is a graph problem, not a scan crash.
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
    // Each phase reads what the one before produced for *every* repo — an
    // include resolves against every repo's nodes, a `depends_on` on another
    // repo's flow against its includes — so they run repo-by-repo inside,
    // phase-by-phase outside.
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

    let workspace_name = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();

    // Sorted by id so node order — and therefore the frontend's computed
    // layout — is stable across polls. `ctx.nodes` is a HashMap, so without
    // this, each `resolve_universe` call could hand back the same nodes in a
    // different order and make the graph visibly jump on every 3s refresh,
    // for reasons unrelated to which flow is selected.
    let mut nodes: Vec<Node> = ctx.nodes.into_values().collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    for node in &mut nodes {
        // Always known once a node's id is — see the field's own doc
        // comment for why this isn't just set at construction time.
        node.domain =
            crate::runs::derive_domain(&node.id, &workspace_name, crate::runs::DomainZone::Http);
    }

    let edges = ctx.edges;

    // Node ids are unique by construction; the names *derived* from them
    // are not. This pass runs here rather than inside the traversal because
    // it needs `node.domain`, which isn't known until the workspace name is.
    // It subsumes the duplicate-`wildcard_hosts` check that used to live
    // inline here — that was one special case of the same collision.
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

// Pulling (`git clone`)-ing missing nodes now happens through
// `downloads::DownloadRegistry`, which runs clones in a background thread and
// streams their output to the UI instead of blocking the request. See
// `src/downloads.rs`.
