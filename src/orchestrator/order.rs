use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::resolver::Edge;

/// Orders `node_ids` so every node's start requirements come first. Runtime
/// edges and edges leaving `node_ids` order nothing. A cycle (which the
/// resolver refuses) is appended in sorted order rather than looping.
pub fn topological_start_order(node_ids: &[String], edges: &[Edge]) -> Vec<String> {
    let ids: HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::new();
    for id in node_ids {
        deps.entry(id.as_str()).or_default();
    }
    for edge in edges {
        if edge.needed_to_start()
            && ids.contains(edge.from.as_str())
            && ids.contains(edge.to.as_str())
        {
            deps.entry(edge.from.as_str())
                .or_default()
                .insert(edge.to.as_str());
        }
    }

    let mut ordered: Vec<String> = Vec::new();
    let mut placed: HashSet<&str> = HashSet::new();
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

/// The members of `ids` something else in `ids` can't start without — the only
/// nodes worth waiting on.
pub fn waited_on(ids: &[String], edges: &[Edge]) -> HashSet<String> {
    let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
    edges
        .iter()
        .filter(|e| e.needed_to_start())
        .filter(|e| set.contains(e.from.as_str()) && set.contains(e.to.as_str()))
        .map(|e| e.to.clone())
        .collect()
}

pub fn requirements_of(id: &str, edges: &[Edge]) -> Vec<String> {
    closure(id, edges, |e| (&e.from, &e.to))
}

pub fn dependents_of(id: &str, edges: &[Edge]) -> Vec<String> {
    closure(id, edges, |e| (&e.to, &e.from))
}

fn closure<'e>(
    id: &str,
    edges: &'e [Edge],
    direction: impl Fn(&'e Edge) -> (&'e String, &'e String),
) -> Vec<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut queue = vec![id.to_string()];
    while let Some(current) = queue.pop() {
        for edge in edges.iter().filter(|e| e.needed_to_start()) {
            let (here, next) = direction(edge);
            if *here == current && next != id && found.insert(next.clone()) {
                queue.push(next.clone());
            }
        }
    }
    let found: Vec<String> = found.into_iter().collect();
    topological_start_order(&found, edges)
}

/// Which nodes failed and which were blocked by a failure. A failure blocks
/// only its required dependents. See `concepts/dependency-kinds.md`.
#[derive(Debug, Default)]
pub struct StartOutcomes {
    requirements: HashMap<String, Vec<String>>,
    failed: BTreeMap<String, String>,
    /// Blocked node → the root failure, not its nearest victim.
    blocked: BTreeMap<String, String>,
}

impl StartOutcomes {
    pub fn new(ids: &[String], edges: &[Edge]) -> Self {
        let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut requirements: HashMap<String, Vec<String>> = HashMap::new();
        for edge in edges.iter().filter(|e| e.needed_to_start()) {
            if set.contains(edge.from.as_str()) && set.contains(edge.to.as_str()) {
                requirements
                    .entry(edge.from.clone())
                    .or_default()
                    .push(edge.to.clone());
            }
        }
        Self {
            requirements,
            ..Self::default()
        }
    }

    /// Call in start order: only sees outcomes already recorded.
    pub fn blocked_by(&self, id: &str) -> Option<&str> {
        let mut causes: Vec<&str> = self
            .requirements
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|dep| {
                if self.failed.contains_key(dep) {
                    Some(dep.as_str())
                } else {
                    self.blocked.get(dep).map(String::as_str)
                }
            })
            .collect();
        causes.sort_unstable();
        causes.first().copied()
    }

    pub fn fail(&mut self, id: &str, error: String) {
        self.failed.insert(id.to_string(), error);
    }

    pub fn block(&mut self, id: &str, by: &str) {
        self.blocked.insert(id.to_string(), by.to_string());
    }

    pub fn error(&self) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .failed
            .iter()
            .map(|(id, error)| {
                let victims: Vec<&str> = self
                    .blocked
                    .iter()
                    .filter(|(_, by)| *by == id)
                    .map(|(v, _)| v.as_str())
                    .collect();
                if victims.is_empty() {
                    format!("{id}: {error}")
                } else {
                    format!("{id}: {error} (blocked: {})", victims.join(", "))
                }
            })
            .collect();
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::testing::{edge, runtime_edge};

    #[test]
    fn topological_start_order_places_dependencies_before_dependents() {
        let ids = vec!["app".to_string(), "db".to_string()];
        let edges = vec![edge("app", "db", "depends-on")];
        let order = topological_start_order(&ids, &edges);
        let db_pos = order.iter().position(|id| id == "db").unwrap();
        let app_pos = order.iter().position(|id| id == "app").unwrap();
        assert!(db_pos < app_pos);
    }

    #[test]
    fn topological_start_order_ignores_edges_outside_the_target_set() {
        let ids = vec!["app".to_string()];
        let edges = vec![edge("app", "not-in-this-run", "depends-on")];
        let order = topological_start_order(&ids, &edges);
        assert_eq!(order, vec!["app".to_string()]);
    }

    #[test]
    fn topological_start_order_ignores_runtime_edges() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let edges = vec![runtime_edge("a", "b")];
        assert_eq!(topological_start_order(&ids, &edges), ids);
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn shop() -> Vec<Edge> {
        vec![
            edge("web", "api", "depends-on"),
            edge("api", "db", "depends-on"),
            edge("worker", "db", "depends-on"),
            runtime_edge("api", "hooks"),
        ]
    }

    #[test]
    fn only_what_something_in_the_run_needs_to_start_is_waited_on() {
        let run = ids(&["web", "api", "db", "hooks"]);
        let mut waited: Vec<String> = waited_on(&run, &shop()).into_iter().collect();
        waited.sort();
        assert_eq!(waited, ids(&["api", "db"]));
    }

    #[test]
    fn requirements_and_dependents_follow_required_edges_only() {
        assert_eq!(requirements_of("web", &shop()), ids(&["db", "api"]));
        assert_eq!(dependents_of("db", &shop()), ids(&["api", "web", "worker"]));
        assert!(dependents_of("hooks", &shop()).is_empty());
    }

    #[test]
    fn a_failure_blocks_its_dependents_transitively_and_nothing_else() {
        let run = ids(&["db", "hooks", "api", "worker", "web"]);
        let mut outcomes = StartOutcomes::new(&run, &shop());
        assert_eq!(outcomes.blocked_by("db"), None);
        outcomes.fail("db", "exited with code 1".into());
        assert_eq!(outcomes.blocked_by("hooks"), None);
        assert_eq!(outcomes.blocked_by("api"), Some("db"));
        outcomes.block("api", "db");
        assert_eq!(outcomes.blocked_by("web"), Some("db"));
        outcomes.block("web", "db");
        assert_eq!(outcomes.blocked_by("worker"), Some("db"));
        outcomes.block("worker", "db");
        assert_eq!(
            outcomes.error().unwrap(),
            "db: exited with code 1 (blocked: api, web, worker)"
        );
    }

    #[test]
    fn a_runtime_dependency_failing_blocks_nothing() {
        let run = ids(&["hooks", "api"]);
        let mut outcomes = StartOutcomes::new(&run, &shop());
        outcomes.fail("hooks", "boom".into());
        assert_eq!(outcomes.blocked_by("api"), None);
        assert_eq!(outcomes.error().unwrap(), "hooks: boom");
    }

    #[test]
    fn nothing_failed_is_no_error() {
        assert!(StartOutcomes::new(&ids(&["db"]), &shop()).error().is_none());
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
