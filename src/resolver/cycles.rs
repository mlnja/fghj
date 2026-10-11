//! Dependency-cycle detection. Blocking: a loop of required edges can never
//! start (`concepts/flows-v2.md`, case 6).

use std::collections::{HashMap, HashSet};

use super::graph::Edge;
use super::warning::Warning;

/// Only [`Edge::needed_to_start`] edges participate.
pub(crate) fn check_cycles(edges: &[Edge]) -> Vec<Warning> {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        if edge.needed_to_start() {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
    }
    // Sorted so reported cycles are stable across runs.
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

    // Rotations of one loop are the same cycle; report it once.
    let mut seen: HashSet<Vec<&str>> = HashSet::new();
    cycles
        .into_iter()
        .filter(|cycle| {
            let mut key = cycle.clone();
            key.sort_unstable();
            seen.insert(key)
        })
        .map(|cycle| {
            Warning::blocking(format!(
                "dependency cycle: {} -> {}. Each waits for the next to start, so none \
                 of them can",
                cycle.join(" -> "),
                cycle[0]
            ))
        })
        .collect()
}

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
    use crate::orchestrator::testing::{edge, runtime_edge};

    #[test]
    fn an_acyclic_graph_reports_nothing() {
        let edges = vec![
            edge("app", "db", "depends-on"),
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

    #[test]
    fn cycles_are_blocking() {
        let edges = vec![edge("a", "b", "depends-on"), edge("b", "a", "depends-on")];
        assert!(check_cycles(&edges)[0].is_blocking());
    }

    #[test]
    fn runtime_edges_do_not_form_a_reportable_cycle() {
        let edges = vec![edge("a", "b", "depends-on"), runtime_edge("b", "a")];
        assert!(check_cycles(&edges).is_empty());
    }

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
