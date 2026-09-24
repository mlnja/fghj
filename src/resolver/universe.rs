//! `resolve_universe` — the orchestrator that scans the workspace, walks
//! every component, then annotates the result with flows and domains.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::git::git_remote_and_branch;
use super::graph::{Graph, Node};
use super::repo_url::normalize_repo_url;
use super::visit::ResolveCtx;
use super::warning::Warning;
use super::workspace_scan::scan_workspace;
use anyhow::Result;

/// Resolves every flow declared by every repo currently on disk in the
/// workspace into one shared graph — the "static full universe" the UI lays
/// out once. Any repo can declare `flows`; there is no distinguished "root"
/// repo. A dependency whose local folder isn't present in the workspace is
/// rendered as a `downloaded: false` stub instead of blocking resolution —
/// call `pull_all` to clone everything reachable and try again.
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

    let mut ctx = ResolveCtx {
        workspace,
        scanned: &scanned,
        repo_index,
        stub_repo_ids: HashMap::new(),
        nodes: HashMap::new(),
        edges: Vec::new(),
        visited: HashSet::new(),
        warnings: Vec::new(),
    };

    // (flow_name, owner_service_id) — the flow's own repo is the BFS root for
    // reachability tagging below. There is no separate "flow" node in the
    // graph: a flow is a named lens over the repo graph, not a repo itself.
    let mut flow_roots: Vec<(String, String)> = Vec::new();
    for (local_path, component) in &scanned {
        for (flow_name, flow) in &component.flows {
            let Some(owner_id) =
                ctx.visit_local_service(local_path, component, flow.service.as_deref())
            else {
                continue;
            };
            flow_roots.push((flow_name.to_string(), owner_id.clone()));

            for dep in flow.dependencies.clone() {
                ctx.visit_dependency(&owner_id, local_path, dep);
            }
        }
    }

    // Every repo actually on disk is part of the map, whether or not any flow
    // reaches it — flows are a highlight overlay, not a visibility filter.
    // This is what makes an entry repo with no flows (or one nothing else
    // references) still show up, along with its own backing/service deps.
    for (local_path, component) in &scanned {
        ctx.visit_local_services(local_path, component);
    }

    // Validate shared-backing references resolve to a real, resolved backing node.
    let known_ids: HashSet<&str> = ctx.nodes.keys().map(|s| s.as_str()).collect();
    for edge in &ctx.edges {
        if edge.kind == "shared-backing" && !known_ids.contains(edge.to.as_str()) {
            // Blocking: the author declared a dependency on a backing node
            // that does not exist, so whatever starts is a graph missing an
            // edge its config says is there.
            ctx.warnings.push(Warning::blocking(format!(
                "dangling shared-backing reference: '{}' points at '{}', which was never resolved as a backing dependency",
                edge.from, edge.to
            )));
        }
    }

    // Per-flow reachability: walk each flow's root over depends-on/owns edges
    // (shared-backing is a cross-reference, not a structural membership edge).
    // Both directions matter: `owner -> dep` (the normal "root needs this")
    // direction, but also `dep -> owner` — e.g. a same-repo sibling service
    // that depends on the flow's root (a dev-server proxying to it) isn't
    // something the root needs, but it's still structurally part of the same
    // component and should highlight with it rather than reading as
    // unrelated. So this treats depends-on/owns as an undirected connectivity
    // graph for membership purposes, while still recording each individual
    // edge (regardless of which way it points) as belonging to the flow.
    let mut adjacency: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
    let mut rev_adjacency: HashMap<&str, Vec<(usize, &str)>> = HashMap::new();
    for (idx, edge) in ctx.edges.iter().enumerate() {
        if edge.kind == "depends-on" || edge.kind == "owns" {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push((idx, edge.to.as_str()));
            rev_adjacency
                .entry(edge.to.as_str())
                .or_default()
                .push((idx, edge.from.as_str()));
        }
    }

    let mut node_flows: HashMap<String, Vec<String>> = HashMap::new();
    let mut edge_flows: Vec<HashSet<String>> = vec![HashSet::new(); ctx.edges.len()];

    for (flow_name, root_id) in &flow_roots {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut queue = vec![root_id.as_str()];
        seen.insert(root_id.as_str());
        node_flows
            .entry(root_id.clone())
            .or_default()
            .push(flow_name.clone());

        while let Some(id) = queue.pop() {
            let forward = adjacency.get(id).into_iter().flatten();
            let backward = rev_adjacency.get(id).into_iter().flatten();
            for &(edge_idx, next) in forward.chain(backward) {
                edge_flows[edge_idx].insert(flow_name.clone());
                if seen.insert(next) {
                    node_flows
                        .entry(next.to_string())
                        .or_default()
                        .push(flow_name.clone());
                    queue.push(next);
                }
            }
        }
    }

    // shared-backing edges are cross-references, not structural edges, so they were
    // excluded from the BFS above — inherit flow membership from their `from` node instead.
    for (edge, flows) in ctx.edges.iter().zip(edge_flows.iter_mut()) {
        if edge.kind == "shared-backing"
            && let Some(fl) = node_flows.get(&edge.from)
        {
            flows.extend(fl.iter().cloned());
        }
    }

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
        if let Some(fl) = node_flows.remove(&node.id) {
            node.flows = fl;
        }
        // Default-run domain, always known once a node's id and
        // domain_scope are — see the field's own doc comment for why this
        // isn't just set at construction time.
        node.domain = crate::runs::derive_domain(
            &node.id,
            &node.domain_scope,
            &workspace_name,
            crate::runs::DEFAULT_RUN_ID,
            crate::runs::DomainZone::Http,
        );
    }

    let mut edges = ctx.edges;
    for (edge, flows) in edges.iter_mut().zip(edge_flows.into_iter()) {
        let mut fl: Vec<String> = flows.into_iter().collect();
        fl.sort();
        edge.flows = fl;
    }

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
        warnings,
    })
}

// Pulling (`git clone`)-ing missing nodes now happens through
// `downloads::DownloadRegistry`, which runs clones in a background thread and
// streams their output to the UI instead of blocking the request. See
// `src/downloads.rs`.
