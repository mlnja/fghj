//! The commands that drive a wired workspace: what the UI does to an
//! environment, from a terminal. Each is the same control-API route the UI
//! calls, so every action has one implementation, in fghjd.
//!
//! Actions reply once fghjd has recorded the intent, so the commands that
//! change something then follow the work until it settles (`follow`).

mod client;
mod follow;
mod logs;
mod status;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use follow::Until;

#[derive(Subcommand)]
pub enum EnvCommand {
    /// Switch the workspace to a flow and wait until it's up. FLOW is
    /// `repo/flow`, or just `flow` when only one repo has it
    Up {
        flow: String,
        #[command(flatten)]
        scope: Scope,
    },
    /// Every node fghj knows about, its state, and where it answers
    Status {
        #[command(flatten)]
        scope: Scope,
    },
    /// Start one node, and whatever it needs. NODE is its id, or the part
    /// before the first `.` when that's unique
    Start {
        node: String,
        #[command(flatten)]
        scope: Scope,
    },
    /// Stop one node, and whatever depends on it
    Stop {
        node: String,
        #[command(flatten)]
        scope: Scope,
    },
    /// Remove one node's container and forget it. Its volumes stay
    Rm {
        node: String,
        #[command(flatten)]
        scope: Scope,
    },
    /// A node's container logs
    Logs {
        node: String,
        /// Keep printing new lines as they come
        #[arg(short, long)]
        follow: bool,
        /// How many lines from the end to start with
        #[arg(long, default_value_t = 200)]
        tail: usize,
        #[command(flatten)]
        scope: Scope,
    },
    /// What fghj did the last time it started (or stopped) a node, step by step
    Events {
        node: String,
        /// The last stop instead of the last start
        #[arg(long)]
        stop: bool,
        #[command(flatten)]
        scope: Scope,
    },
    /// Clone the repos a flow, or one node, needs and the workspace lacks.
    /// Without either, every repo the workspace references
    Pull {
        node: Option<String>,
        #[arg(long, conflicts_with = "node")]
        flow: Option<String>,
        #[command(flatten)]
        scope: Scope,
    },
    /// Halt a node before its own code runs, until a debugger attaches
    /// (`FGHJ_DEBUG_WAIT`). Recreates the node
    DebugWait {
        node: String,
        state: Switch,
        #[command(flatten)]
        scope: Scope,
    },
    /// The wired workspaces
    Workspaces {
        #[command(subcommand)]
        action: Option<WorkspacesAction>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum WorkspacesAction {
    /// Unwire a workspace and remove its containers and network. Its
    /// checkouts and volumes stay
    Rm { id: String },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

#[derive(Args)]
pub struct Scope {
    /// Workspace root (default: the wired workspace the current directory is
    /// inside)
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Print fghjd's JSON instead of text
    #[arg(long)]
    json: bool,
}

pub async fn run(command: EnvCommand) -> Result<()> {
    match command {
        EnvCommand::Up { flow, scope } => up(&flow, scope).await,
        EnvCommand::Status { scope } => {
            let ws = workspace_id(scope.workspace).await?;
            print_status(&ws, scope.json).await
        }
        EnvCommand::Start { node, scope } => node_action(&node, "start", scope).await,
        EnvCommand::Stop { node, scope } => node_action(&node, "stop", scope).await,
        EnvCommand::Rm { node, scope } => node_action(&node, "delete", scope).await,
        EnvCommand::Logs {
            node,
            follow,
            tail,
            scope,
        } => {
            let ws = workspace_id(scope.workspace).await?;
            let node = known_node(&ws, &node).await?;
            logs::print(&ws, &node, tail, follow, scope.json).await
        }
        EnvCommand::Events { node, stop, scope } => {
            let ws = workspace_id(scope.workspace).await?;
            let node = known_node(&ws, &node).await?;
            events(&ws, &node, if stop { "stop" } else { "start" }, scope.json).await
        }
        EnvCommand::Pull { node, flow, scope } => pull(node, flow, scope).await,
        EnvCommand::DebugWait { node, state, scope } => {
            let ws = workspace_id(scope.workspace).await?;
            let node = known_node(&ws, &node).await?;
            let after = follow::latest(&ws).await?;
            let wait = matches!(state, Switch::On);
            client::post(
                &format!("/nodes/{node}/debug-wait?workspace={ws}"),
                &json!({ "wait": wait }),
            )
            .await?;
            follow::follow(&ws, after, Until::Settled, scope.json).await?;
            finish(&ws, scope.json).await
        }
        EnvCommand::Workspaces { action, json } => match action {
            None => workspaces(json).await,
            Some(WorkspacesAction::Rm { id }) => {
                let job = client::post("/workspaces/remove", &json!({ "id": id })).await?;
                let job_id = job["id"].as_u64().context("fghjd didn't say which job")?;
                follow::follow(&id, job_id - 1, Until::Closed(job_id), json).await
            }
        },
    }
}

/// The id of the workspace a command is about. Shared with `exec`.
pub async fn workspace_id(flag: Option<PathBuf>) -> Result<String> {
    let root = crate::workspace_root(flag)?;
    let workspaces = client::get("/workspaces").await?;
    workspaces
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| w["workspace"].as_str().map(PathBuf::from).as_ref() == Some(&root))
        .and_then(|w| w["id"].as_str())
        .map(str::to_string)
        .with_context(|| {
            format!(
                "{} isn't wired into fghjd — run `fghj wire <repo>` first",
                root.display()
            )
        })
}

async fn universe(ws: &str) -> Result<Value> {
    client::get(&format!("/universe.json?workspace={ws}")).await
}

async fn up(flow: &str, scope: Scope) -> Result<()> {
    let ws = workspace_id(scope.workspace).await?;
    let flow = pick_flow(&universe(&ws).await?, flow)?;
    let after = follow::latest(&ws).await?;
    client::post(&format!("/switch?workspace={ws}"), &json!({ "flow": flow })).await?;
    follow::follow(&ws, after, Until::Settled, scope.json).await?;
    finish(&ws, scope.json).await
}

async fn node_action(node: &str, action: &str, scope: Scope) -> Result<()> {
    let ws = workspace_id(scope.workspace).await?;
    let node = if action == "start" {
        universe_node(&ws, node).await?
    } else {
        known_node(&ws, node).await?
    };
    let after = follow::latest(&ws).await?;
    client::post(
        &format!("/nodes/{node}/{action}?workspace={ws}"),
        &json!({}),
    )
    .await?;
    follow::follow(&ws, after, Until::Settled, scope.json).await?;
    finish(&ws, scope.json).await
}

/// After an action: where things stand now.
async fn finish(ws: &str, json: bool) -> Result<()> {
    if !json {
        println!();
    }
    print_status(ws, json).await
}

async fn print_status(ws: &str, json: bool) -> Result<()> {
    let env = client::get(&format!("/environment?workspace={ws}")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&env)?);
        return Ok(());
    }
    let containers = env["containers"].as_object();
    if containers.is_none_or(|c| c.is_empty()) {
        let universe = universe(ws).await?;
        println!("nothing has run in this workspace yet. Its flows:");
        for flow in flows(&universe) {
            println!("  {flow}");
        }
        println!("\nstart one with `fghj up <flow>`");
        return Ok(());
    }
    print!("{}", status::table(&env));
    Ok(())
}

/// A node fghj has a container record for: stop, rm, logs and the rest
/// act on one.
async fn known_node(ws: &str, node: &str) -> Result<String> {
    let env = client::get(&format!("/environment?workspace={ws}")).await?;
    let ids: Vec<&str> = env["containers"]
        .as_object()
        .into_iter()
        .flat_map(|c| c.keys())
        .map(String::as_str)
        .collect();
    pick(node, &ids, short_node, "node").map_err(|e| {
        if ids.is_empty() {
            anyhow::anyhow!("nothing has run in this workspace yet — `fghj up <flow>` first")
        } else {
            e
        }
    })
}

/// Any node in the workspace's graph: start can create one.
async fn universe_node(ws: &str, node: &str) -> Result<String> {
    let universe = universe(ws).await?;
    let ids: Vec<&str> = universe["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["id"].as_str())
        .collect();
    pick(node, &ids, short_node, "node")
}

fn flows(universe: &Value) -> Vec<&str> {
    universe["flows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn pick_flow(universe: &Value, flow: &str) -> Result<String> {
    pick(flow, &flows(universe), short_flow, "flow")
}

fn short_flow(flow: &str) -> Option<&str> {
    flow.rsplit_once('/').map(|(_, name)| name)
}

fn short_node(id: &str) -> Option<&str> {
    id.split_once('.').map(|(name, _)| name)
}

/// `wanted` exactly, or the one candidate whose `short` form it is. Several
/// matches are an error naming them all, not a guess.
fn pick<'a>(
    wanted: &str,
    candidates: &[&'a str],
    short: impl Fn(&'a str) -> Option<&'a str>,
    what: &str,
) -> Result<String> {
    if let Some(exact) = candidates.iter().find(|c| **c == wanted) {
        return Ok(exact.to_string());
    }
    let matches: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|c| short(c) == Some(wanted))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.to_string()),
        [] => {
            let mut known = candidates.to_vec();
            known.sort();
            bail!("no {what} {wanted}; there's {}", known.join(", "))
        }
        several => bail!("{wanted} could be {}; name one", several.join(" or ")),
    }
}

async fn events(ws: &str, node: &str, action: &str, json: bool) -> Result<()> {
    let resp = client::get(&format!(
        "/nodes/{node}/events?workspace={ws}&action={action}"
    ))
    .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&resp["events"])?);
        return Ok(());
    }
    let events = resp["events"].as_array().cloned().unwrap_or_default();
    if events.is_empty() {
        println!("fghj hasn't recorded a {action} of {node}");
        return Ok(());
    }
    print!("{}", status::events(&events));
    Ok(())
}

async fn pull(node: Option<String>, flow: Option<String>, scope: Scope) -> Result<()> {
    let ws = workspace_id(scope.workspace).await?;
    let job = match (node, flow) {
        (Some(node), _) => {
            let node = universe_node(&ws, &node).await?;
            client::post(&format!("/pull/{node}?workspace={ws}"), &json!({})).await?
        }
        (None, flow) => {
            let query = match flow {
                Some(flow) => format!("&flow={}", pick_flow(&universe(&ws).await?, &flow)?),
                None => String::new(),
            };
            client::post(&format!("/pull-all?workspace={ws}{query}"), &json!({})).await?
        }
    };
    let id = job["id"].as_u64().context("fghjd didn't say which job")?;
    follow::follow(&ws, id - 1, Until::Closed(id), scope.json).await
}

async fn workspaces(json: bool) -> Result<()> {
    let list = client::get("/workspaces").await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }
    let list = list.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no workspaces are wired — `fghj wire <repo>` wires one");
    }
    for w in list {
        println!(
            "{}  {}",
            w["id"].as_str().unwrap_or("?"),
            w["workspace"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODES: &[&str] = &["api.files-api", "db.files-api", "api.files-worker"];

    #[test]
    fn an_exact_id_always_wins() {
        assert_eq!(
            pick("db.files-api", NODES, short_node, "node").unwrap(),
            "db.files-api"
        );
    }

    #[test]
    fn a_unique_short_name_picks_its_node() {
        assert_eq!(
            pick("db", NODES, short_node, "node").unwrap(),
            "db.files-api"
        );
    }

    #[test]
    fn an_ambiguous_short_name_names_every_match() {
        let err = pick("api", NODES, short_node, "node")
            .unwrap_err()
            .to_string();
        assert!(err.contains("api.files-api or api.files-worker"), "{err}");
    }

    #[test]
    fn an_unknown_name_lists_what_there_is() {
        let err = pick("web", NODES, short_node, "node")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("api.files-api, api.files-worker, db.files-api"),
            "{err}"
        );
    }

    #[test]
    fn a_flow_is_named_with_or_without_its_repo() {
        let flows = ["files-api/app", "files-api/demo", "files-web/app"];
        assert_eq!(
            pick("demo", &flows, short_flow, "flow").unwrap(),
            "files-api/demo"
        );
        assert_eq!(
            pick("files-web/app", &flows, short_flow, "flow").unwrap(),
            "files-web/app"
        );
        assert!(pick("app", &flows, short_flow, "flow").is_err());
    }
}
