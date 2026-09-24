//! Dependency-cycle detection.
//!
//! [[branch-ownership-model]] acknowledges outright that real requirement
//! cycles between repos are possible, and `runs::topological_start_order`
//! already handles one without hanging: when no node can make progress, the
//! remainder is appended in sorted order and the run starts anyway. That
//! fallback is the right call — refusing to bring up an entire environment
//! over a cycle would be worse than bringing it up in an arbitrary order.
//!
//! What was missing is that nobody was ever told. The start order silently
//! stops being a dependency order, so a service can come up before the
//! database it declared it needs, and the only symptom is a crash loop with
//! no explanation. The graph *layout* code defends against cycles (it drops
//! back-edges so the canvas doesn't grow without bound); the code that
//! actually starts containers did not even mention them.
//!
//! So: advisory, not blocking. Report the cycle, name the nodes in it, and
//! let the run start.

use std::collections::{HashMap, HashSet};

use super::graph::Edge;
use super::warning::Warning;

/// One warning per distinct cycle, naming the nodes in the order they
/// depend on each other so the message reads as the loop it is
/// (`a -> b -> c -> a`) rather than as an unordered set.
///
/// Only `depends-on`/`owns` edges participate: `shared-backing` is a
/// cross-reference, not a structural requirement, and including it would
/// report cycles that never affect start order (which is derived from the
/// same two kinds — see `runs::topological_start_order`).
pub(crate) fn check_cycles(edges: &[Edge]) -> Vec<Warning> {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        if edge.kind == "depends-on" || edge.kind == "owns" {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
    }
    // Sorted so the set of cycles reported — and which node each is rooted
    // at — is stable across runs, the same reason `topological_start_order`
    // sorts its fallback.
    for targets in adjacency.values_mut() {
        targets.sort_unstable();
        targets.dedup();
    }
    let mut roots: Vec<&str> = adjacency.keys().copied().collect();
    roots.sort_unstable();

    let mut done: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut cycles: Vec<Vec<&str>> = Vec::new();

    for root in roots {
        if !done.contains(root) {
            walk(
                root,
                &adjacency,
                &mut done,
                &mut stack,
                &mut on_stack,
                &mut cycles,
            );
        }
    }

    // Two back-edges around the same loop describe the same cycle rotated;
    // canonicalize by sorted node set so it is reported once.
    let mut seen: HashSet<Vec<&str>> = HashSet::new();
    cycles
        .into_iter()
        .filter(|cycle| {
            let mut key = cycle.clone();
            key.sort_unstable();
            seen.insert(key)
        })
        .map(|cycle| {
            Warning::advisory(format!(
                "dependency cycle: {} -> {}. Start order can't honour it, so these \
                 nodes come up in an arbitrary (but stable) order",
                cycle.join(" -> "),
                cycle[0]
            ))
        })
        .collect()
}

/// Plain iterative-friendly recursive DFS: a back-edge to a node still on
/// the stack is a cycle, and the cycle is the stack from that node onward.
fn walk<'a>(
    id: &'a str,
    adjacency: &HashMap<&'a str, Vec<&'a str>>,
    done: &mut HashSet<&'a str>,
    stack: &mut Vec<&'a str>,
    on_stack: &mut HashSet<&'a str>,
    cycles: &mut Vec<Vec<&'a str>>,
) {
    stack.push(id);
    on_stack.insert(id);
    for &next in adjacency.get(id).into_iter().flatten() {
        if on_stack.contains(next) {
            let at = stack.iter().position(|n| *n == next).expect("on the stack");
            cycles.push(stack[at..].to_vec());
        } else if !done.contains(next) {
            walk(next, adjacency, done, stack, on_stack, cycles);
        }
    }
    stack.pop();
    on_stack.remove(id);
    done.insert(id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::testing::edge;

    #[test]
    fn an_acyclic_graph_reports_nothing() {
        let edges = vec![
            edge("app", "db", "owns"),
            edge("app", "cache", "depends-on"),
        ];
        assert!(check_cycles(&edges).is_empty());
    }

    #[test]
    fn a_two_node_cycle_is_reported_once_not_twice() {
        let edges = vec![edge("a", "b", "depends-on"), edge("b", "a", "depends-on")];
        let warnings = check_cycles(&edges);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].message.contains("a -> b -> a"), "{warnings:?}");
    }

    #[test]
    fn a_longer_cycle_is_reported_in_dependency_order() {
        let edges = vec![
            edge("a", "b", "depends-on"),
            edge("b", "c", "depends-on"),
            edge("c", "a", "depends-on"),
        ];
        let warnings = check_cycles(&edges);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("a -> b -> c -> a"),
            "{warnings:?}"
        );
    }

    /// A cycle can't stop a run — `topological_start_order` deliberately
    /// falls back rather than failing, and this exists to explain that, not
    /// to override it.
    #[test]
    fn cycles_are_advisory() {
        let edges = vec![edge("a", "b", "depends-on"), edge("b", "a", "depends-on")];
        assert!(!check_cycles(&edges)[0].is_blocking());
    }

    /// `shared-backing` is a cross-reference the start order already
    /// ignores, so a loop through one isn't a start-order problem.
    #[test]
    fn shared_backing_edges_do_not_form_a_reportable_cycle() {
        let edges = vec![
            edge("a", "b", "depends-on"),
            edge("b", "a", "shared-backing"),
        ];
        assert!(check_cycles(&edges).is_empty());
    }

    /// Two independent cycles are two findings, not one merged blob.
    #[test]
    fn disjoint_cycles_are_reported_separately() {
        let edges = vec![
            edge("a", "b", "depends-on"),
            edge("b", "a", "depends-on"),
            edge("x", "y", "depends-on"),
            edge("y", "x", "depends-on"),
        ];
        assert_eq!(check_cycles(&edges).len(), 2);
    }
}
