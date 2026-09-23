use std::collections::{HashMap, HashSet};

use crate::resolver::Edge;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::testing::edge;

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
}
