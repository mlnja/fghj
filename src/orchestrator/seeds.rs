//! Whether a seed has already filled the data its stores hold now. See
//! `concepts/seeds-and-migrations.md`.

use super::naming::container_name;
use crate::daemon_log;
use crate::docker;
use crate::resolver::{Graph, Node};

use super::registry::Orchestrator;

impl Orchestrator {
    /// Every store's data, in one string a record can be compared with. `None`
    /// when a store can't be read: then the seed runs, rather than being
    /// skipped on a guess.
    async fn seeded_data(&self, graph: &Graph, node: &Node) -> Option<String> {
        let mut parts = Vec::new();
        for store in &node.seeds {
            let name = container_name(&graph.workspace_name, store);
            let identity = docker::data_identity(&self.docker, &name).await.ok()??;
            parts.push(format!("{store}={}", identity.join(",")));
        }
        Some(parts.join(";"))
    }

    /// Narrated as the start it replaces, so the drawer says why nothing ran.
    pub(super) async fn already_seeded(&self, graph: &Graph, node: &Node) -> bool {
        let Some(data) = self.seeded_data(graph, node).await else {
            return false;
        };
        let Ok(Some(seeded)) = self.db.clone().load_seed(node.id.clone()).await else {
            return false;
        };
        if seeded.data != data {
            return false;
        }
        let ev = self.events(&node.id, "start");
        ev.begin().await;
        ev.record(
            "seeding",
            "ok",
            Some(format!(
                "skipped: {} still hold{} the data this seeded {}",
                node.seeds.join(", "),
                if node.seeds.len() == 1 { "s" } else { "" },
                ago(now_secs() - seeded.at)
            )),
        )
        .await;
        true
    }

    /// After a clean exit only: a failed seed is never done.
    pub(super) async fn record_seeded(&self, graph: &Graph, node: &Node) {
        let Some(data) = self.seeded_data(graph, node).await else {
            daemon_log::warn(format!(
                "fghjd: {} finished, but its stores couldn't be read, so it runs again next start",
                node.id
            ));
            return;
        };
        if let Err(e) = self.db.clone().save_seed(node.id.clone(), data).await {
            daemon_log::warn(format!(
                "fghjd: {} finished, but recording it failed, so it runs again next start: {e:#}",
                node.id
            ));
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ago(secs: i64) -> String {
    let (n, unit) = match secs {
        ..60 => return "just now".to_string(),
        60..3600 => (secs / 60, "minute"),
        3600..86400 => (secs / 3600, "hour"),
        _ => (secs / 86400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn how_long_ago_reads_in_the_largest_whole_unit() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(7200), "2 hours ago");
        assert_eq!(ago(3 * 86400 + 5), "3 days ago");
    }

    fn docker(args: &[&str]) {
        let status = std::process::Command::new("docker")
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("failed to run docker");
        assert!(status.success(), "docker {args:?}");
    }

    #[tokio::test]
    #[ignore = "needs a Docker daemon: see concepts/release-and-delivery.md"]
    async fn a_seed_is_done_until_its_store_gets_new_data() {
        use crate::orchestrator::testing::{test_graph, test_node};
        use crate::persistence::WorkspaceDb;
        use std::sync::Arc;

        let workspace = format!("seedtest{}", crate::docker::rand_suffix());
        let mut seed = test_node("seed.api", "seed", "task");
        seed.seeds = vec!["db.api".to_string()];
        let mut graph = test_graph(
            vec![test_node("db.api", "db", "backing"), seed.clone()],
            Vec::new(),
        );
        graph.workspace_name = workspace.clone();
        let store = container_name(&workspace, "db.api");
        let volume = format!("{store}-data");
        let run_store = || {
            docker(&[
                "run",
                "-d",
                "--name",
                &store,
                "-v",
                &format!("{volume}:/data"),
                "busybox",
                "sleep",
                "60",
            ])
        };

        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(WorkspaceDb::open(tmp.path()).unwrap());
        let client = Arc::new(crate::daemon::connect_docker().expect("docker client"));
        let orchestrator = Orchestrator::new(tmp.path().to_path_buf(), db, client);

        run_store();
        assert!(
            !orchestrator.already_seeded(&graph, &seed).await,
            "never seeded"
        );
        orchestrator.record_seeded(&graph, &seed).await;
        assert!(orchestrator.already_seeded(&graph, &seed).await);

        docker(&["rm", "-f", &store]);
        run_store();
        assert!(
            orchestrator.already_seeded(&graph, &seed).await,
            "same volume, same data"
        );

        docker(&["rm", "-f", &store]);
        docker(&["volume", "rm", &volume]);
        run_store();
        let fresh = orchestrator.already_seeded(&graph, &seed).await;
        docker(&["rm", "-f", &store]);
        docker(&["volume", "rm", &volume]);
        assert!(!fresh, "a volume made again is new data");
    }
}
