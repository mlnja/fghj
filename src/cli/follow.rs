//! Watching fghjd's background work from a terminal. An action's reply only
//! means the intent is recorded; this prints the activity log as the work
//! goes, and returns once it has settled. Ctrl-C stops the watching, not the
//! work.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use super::client;

const POLL: Duration = Duration::from_millis(300);

pub enum Until {
    /// Nothing pending in the environment: what a switch or a node action
    /// waits for. fghjd settles an action's activity before it clears the
    /// pending mark, so by then its log is complete.
    Settled,
    /// That one activity has finished: a pull or a removal, which leave the
    /// environment alone.
    Closed(u64),
}

/// The newest activity id in `ws`, so `follow` can tell which ones are new.
pub async fn latest(ws: &str) -> Result<u64> {
    let list = client::get(&format!("/activities?workspace={ws}")).await?;
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["id"].as_u64())
        .max()
        .unwrap_or(0))
}

/// Prints each new top-level activity's log as it grows (its children's
/// lines are already in it), then fails if any of them did. `quiet` prints
/// nothing, for `--json`.
pub async fn follow(ws: &str, after: u64, until: Until, quiet: bool) -> Result<()> {
    let mut printed: BTreeMap<u64, usize> = BTreeMap::new();
    loop {
        let list = client::get(&format!("/activities?workspace={ws}")).await?;
        let roots = new_roots(&list, after);
        for activity in &roots {
            let id = activity["id"].as_u64().unwrap_or_default();
            let log = activity["log"].as_str().unwrap_or_default();
            let done = printed.entry(id).or_default();
            // Whole lines only; a partial one is printed when it's finished.
            let end = log.rfind('\n').map_or(0, |i| i + 1);
            if !quiet && end > *done {
                print!("{}", &log[*done..end]);
            }
            *done = (*done).max(end);
        }
        let open = roots.iter().any(|a| is_open(a));
        let finished = match until {
            Until::Settled => !open && settled(ws).await?,
            Until::Closed(id) => roots
                .iter()
                .any(|a| a["id"].as_u64() == Some(id) && !is_open(a)),
        };
        if finished {
            let failed: Vec<&str> = roots
                .iter()
                .filter(|a| a["status"] == "error")
                .filter_map(|a| a["key"].as_str())
                .collect();
            if !failed.is_empty() {
                bail!("{} failed", failed.join(", "));
            }
            return Ok(());
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Oldest first, so the logs print in the order the work started.
fn new_roots(list: &Value, after: u64) -> Vec<&Value> {
    let mut roots: Vec<&Value> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["parent"].is_null() && a["id"].as_u64().is_some_and(|id| id > after))
        .collect();
    roots.sort_by_key(|a| a["id"].as_u64());
    roots
}

fn is_open(activity: &Value) -> bool {
    matches!(activity["status"].as_str(), Some("queued" | "running"))
}

async fn settled(ws: &str) -> Result<bool> {
    let env = client::get(&format!("/environment?workspace={ws}")).await?;
    Ok(is_settled(&env))
}

fn is_settled(env: &Value) -> bool {
    env["pending_create"].is_null()
        && env["containers"]
            .as_object()
            .is_none_or(|c| c.values().all(|c| c["pending_action"].is_null()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_new_top_level_activities_are_followed_oldest_first() {
        let list = json!([
            { "id": 9, "parent": null },
            { "id": 8, "parent": 7 },
            { "id": 7, "parent": null },
            { "id": 3, "parent": null },
        ]);
        let ids: Vec<u64> = new_roots(&list, 5)
            .iter()
            .filter_map(|a| a["id"].as_u64())
            .collect();
        assert_eq!(ids, [7, 9]);
    }

    #[test]
    fn a_pending_create_or_node_action_is_not_settled() {
        assert!(is_settled(
            &json!({ "pending_create": null, "containers": {} })
        ));
        assert!(!is_settled(
            &json!({ "pending_create": { "flow": "a/b" }, "containers": {} })
        ));
        assert!(!is_settled(&json!({
            "pending_create": null,
            "containers": { "db": { "pending_action": "stopping" } },
        })));
        // No environment at all: nothing can be pending.
        assert!(is_settled(&Value::Null));
    }
}
