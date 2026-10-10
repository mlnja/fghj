//! Shared fixtures for the `runs` submodule tests.

use std::collections::BTreeMap;

use crate::resolver::{Edge, Graph, Node};

/// A `required: false` "depends-on" edge: needed at runtime, not to start.
pub fn runtime_edge(from: &str, to: &str) -> Edge {
    Edge {
        required: false,
        ..edge(from, to, "depends-on")
    }
}

pub fn edge(from: &str, to: &str, kind: &str) -> Edge {
    Edge {
        from: from.to_string(),
        to: to.to_string(),
        kind: kind.to_string(),
        required: kind == "depends-on",
        condition: None,
        via_flow: None,
        flows: Vec::new(),
    }
}

pub fn test_node(id: &str, label: &str, kind: &str) -> Node {
    Node {
        id: id.to_string(),
        label: label.to_string(),
        kind: kind.to_string(),
        image: None,
        branch: None,
        repo: None,
        // Ids are `{name}.{local_path}`, so the repo is everything after
        // the first dot.
        local_path: id.split_once('.').map(|(_, repo)| repo.to_string()),
        domain: String::new(),
        downloaded: true,
        dirty: false,
        head: None,
        flows: Vec::new(),
        includes: BTreeMap::new(),
        build: None,
        ports: BTreeMap::new(),
        environment: Vec::new(),
        command: Vec::new(),
        volumes: Vec::new(),
        additional_hosts: Vec::new(),
        wildcard_hosts: Vec::new(),
        env_file: Vec::new(),
        restart: "no".to_string(),
        stop_signal: None,
        stop_grace_period: 10,
        user: None,
        working_dir: None,
        labels: BTreeMap::new(),
        cap_add: Vec::new(),
        cap_drop: Vec::new(),
        privileged: false,
        extra_hosts: Vec::new(),
        healthcheck: None,
        platform: None,
        run_policy: None,
        debug: None,
    }
}

pub fn test_graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
    Graph {
        workspace_name: "shop".to_string(),
        nodes,
        edges,
        flows: Vec::new(),
        warnings: Vec::new(),
    }
}
