//! Whole-run orchestration: starting a run and bringing it up to date.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

use super::health::HealthBudget;
use super::naming::{DEFAULT_RUN_ID, resolve_run_id};
use super::order::topological_start_order;
use super::progress::{ProgressSink, RunProgress, report};
use crate::docker;
use crate::resolver::{Graph, Node};
use crate::state::{RunCreateError, RunSpec, RunState};
use crate::util::label::sanitize_label;

use super::registry::RunRegistry;

impl RunRegistry {
    /// `prior` is whatever the reducer already had for this run id, if
    /// anything — passed in rather than looked up, since the reducer owns
    /// the only copy. A named run always starts clean, so an existing one
    /// is torn down first.
    pub async fn start(
        &self,
        graph: &Graph,
        spec: RunSpec,
        prior: Option<&RunState>,
        progress: Option<&ProgressSink>,
    ) -> Result<RunState> {
        let run_id = resolve_run_id(spec.run_id.as_deref());

        // starting an already-running run replaces it cleanly
        if let Some(prior) = prior {
            self.stop(&run_id, prior).await?;
        }

        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;

        let (sidecar_container_name, sidecar_ip) = match self
            .ensure_sidecar(&graph.workspace_name, &run_id, &network)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                docker::remove_network(&self.docker, &network).await;
                return Err(e);
            }
        };

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| {
                spec.flow
                    .as_deref()
                    .is_none_or(|flow| n.flows.iter().any(|f| f == flow))
            })
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);

        let mut containers = BTreeMap::new();
        // One budget for the whole run, not one per node: nodes start
        // sequentially, so a per-node limit bounded nothing an impatient
        // person cares about. See `HealthBudget`.
        let budget = HealthBudget::default();
        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            match self
                .start_node(graph, node, &run_id, &network, Some(&sidecar_ip), &budget)
                .await
            {
                Ok(info) => {
                    // Reported before being folded into the local map so a
                    // daemon that dies on the *next* node still leaves a
                    // record of this one.
                    report(
                        progress,
                        RunProgress {
                            run_id: run_id.clone(),
                            network: network.clone(),
                            sidecar_container_name: sidecar_container_name.clone(),
                            sidecar_ip: Some(sidecar_ip.clone()),
                            info: info.clone(),
                        },
                    );
                    containers.insert(info.node_id.clone(), info);
                }
                Err(e) => {
                    for c in containers.values() {
                        docker::stop_and_remove(&self.docker, &c.desired.container_name).await;
                    }
                    docker::stop_and_remove(&self.docker, &sidecar_container_name).await;
                    docker::remove_network(&self.docker, &network).await;
                    return Err(e);
                }
            }
        }

        let state = RunState {
            run_id: run_id.clone(),
            network,
            containers,
            sidecar_container_name,
            sidecar_ip: Some(sidecar_ip),
            ..Default::default()
        };
        Ok(state)
    }

    /// Tops up the single default environment so every node reachable from
    /// `flow` (or every node in the graph, if `flow` is `None`) is running —
    /// unlike `start`, this never touches a container that's already alive.
    /// fghj models one shared set of running containers per workspace, not a
    /// separate environment per flow, so picking a flow should never restart
    /// (or duplicate) whatever's already up.
    ///
    /// Liveness is checked directly against docker on every call rather than
    /// trusting the persisted `RunState`, since a container can be
    /// stopped/removed out-of-band between calls (see `refresh`).
    ///
    /// Deliberately does *not* roll back the way `start` does when a node
    /// fails partway: this tops up the one shared default environment, so
    /// tearing down the three containers that came up because the fourth
    /// didn't would destroy exactly the progress the per-node reporting
    /// below exists to keep. Each node is reported through `progress` as it
    /// comes up, and the error additionally carries the whole partial state
    /// (`RunCreateError::partial`), so the containers stay visible and
    /// routable instead of running unseen.
    pub async fn ensure_running(
        &self,
        graph: &Graph,
        flow: Option<&str>,
        prior: Option<&RunState>,
        progress: Option<&ProgressSink>,
    ) -> Result<RunState, RunCreateError> {
        let run_id = DEFAULT_RUN_ID.to_string();
        let network = format!("fghj-{}-{}", sanitize_label(&graph.workspace_name), run_id);
        docker::ensure_network(&self.docker, &network, &network).await?;
        let (sidecar_container_name, sidecar_ip) = self
            .ensure_sidecar(&graph.workspace_name, &run_id, &network)
            .await?;

        let mut state = prior.cloned().unwrap_or_else(|| RunState {
            run_id: run_id.clone(),
            network: network.clone(),
            sidecar_container_name: sidecar_container_name.clone(),
            sidecar_ip: Some(sidecar_ip.clone()),
            ..Default::default()
        });
        state.sidecar_container_name = sidecar_container_name;
        state.sidecar_ip = Some(sidecar_ip);

        let node_map: HashMap<&str, &Node> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let target_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|n| n.kind != "flow")
            .filter(|n| flow.is_none_or(|flow| n.flows.iter().any(|f| f == flow)))
            .map(|n| n.id.clone())
            .collect();
        let ordered_ids = topological_start_order(&target_ids, &graph.edges);
        let budget = HealthBudget::default();

        for node_id in &ordered_ids {
            let node = node_map[node_id.as_str()];
            let container_name = format!(
                "fghj-{}-{}-{}",
                sanitize_label(&graph.workspace_name),
                run_id,
                sanitize_label(&node.id)
            );
            let alive = matches!(
                docker::inspect_status(&self.docker, &container_name, "").await,
                Ok(Some(s)) if s.status == "running"
            );
            // Alive alone isn't enough to skip: `state.containers` (loaded
            // from `self.runs`, itself loaded from the DB — see `new`'s
            // reconciliation, which drops a whole run's history the moment
            // any single one of its containers isn't found) can be missing
            // this node's `ContainerInfo`/routes even though the container
            // itself is still running fine. Falling through and recreating
            // it is how it gets re-described (and its routes re-registered
            // in the route table below) rather than staying silently
            // unrouted until something else happens to bounce it.
            if alive && state.containers.contains_key(&node.id) {
                continue;
            }
            // A stopped-but-not-removed container from a previous run would
            // otherwise collide with create_container's fixed name.
            docker::stop_and_remove(&self.docker, &container_name).await;

            let info = match self
                .start_node(
                    graph,
                    node,
                    &run_id,
                    &network,
                    state.sidecar_ip.as_deref(),
                    &budget,
                )
                .await
            {
                Ok(info) => info,
                Err(e) => {
                    return Err(RunCreateError {
                        message: format!("{e:#}"),
                        partial: Some(state),
                    });
                }
            };
            // Reported after every node, not just at the end, so a later
            // failure — or a daemon that dies outright — doesn't lose track
            // of containers that did start successfully.
            report(
                progress,
                RunProgress {
                    run_id: run_id.clone(),
                    network: network.clone(),
                    sidecar_container_name: state.sidecar_container_name.clone(),
                    sidecar_ip: state.sidecar_ip.clone(),
                    info: info.clone(),
                },
            );
            state.containers.insert(info.node_id.clone(), info);
        }

        Ok(state)
    }
}
