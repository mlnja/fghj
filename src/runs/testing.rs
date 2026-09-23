//! Shared fixtures for the `runs` submodule tests.

use std::collections::BTreeMap;

use crate::resolver::{Edge, Graph, Node};

pub fn edge(from: &str, to: &str, kind: &str) -> Edge {
    Edge {
        from: from.to_string(),
        to: to.to_string(),
        kind: kind.to_string(),
        branch: None,
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
        domain_scope: "run".to_string(),
        local_path: None,
        domain: String::new(),
        downloaded: true,
        dirty: false,
        flows: Vec::new(),
        build: None,
        ports: BTreeMap::new(),
        environment: Vec::new(),
        command: Vec::new(),
        volumes: Vec::new(),
        additional_hosts: Vec::new(),
        wildcard_hosts: Vec::new(),
        env_file: Vec::new(),
        restart: "no".to_string(),
        user: None,
        working_dir: None,
        labels: BTreeMap::new(),
        cap_add: Vec::new(),
        cap_drop: Vec::new(),
        privileged: false,
        extra_hosts: Vec::new(),
        healthcheck: None,
        platform: None,
    }
}

pub fn test_graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
    Graph {
        workspace_name: "shop".to_string(),
        nodes,
        edges,
        warnings: Vec::new(),
    }
}
