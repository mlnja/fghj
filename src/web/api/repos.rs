//! The Repos tab: git state of each checkout, and fetching one.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::Json;
use axum::extract::Path as AxumPath;
use axum::response::{IntoResponse, Response};

use crate::resolver::git::{RepoStatus, repo_status};
use crate::server::WorkspaceState;
use crate::web::api::error::{bad_request, err_response};
use crate::web::api::extract::WorkspaceExtractor;
use crate::{downloads, persistence, resolver};

/// Long enough for a big first fetch, short enough that a hung remote
/// doesn't leave the button spinning forever.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// The checkouts the workspace's graph knows of, by `local_path`. Taken from
/// the graph rather than the folder, so a path from the client can't name
/// anything else.
async fn checkouts(state: &WorkspaceState) -> anyhow::Result<BTreeSet<String>> {
    let graph = resolver::resolve_universe_async(state.path.clone()).await?;
    Ok(graph
        .nodes
        .into_iter()
        .filter(|n| n.downloaded)
        .filter_map(|n| n.local_path)
        .collect())
}

/// `local_path` to its status, `null` for one git couldn't read.
pub(crate) async fn get_repos(WorkspaceExtractor(state): WorkspaceExtractor) -> Response {
    let paths = match checkouts(&state).await {
        Ok(paths) => paths,
        Err(e) => return err_response(e),
    };
    let root = state.path.clone();
    let statuses = tokio::task::spawn_blocking(move || {
        // Three git calls each, so in parallel.
        std::thread::scope(|scope| {
            let handles: Vec<_> = paths
                .into_iter()
                .map(|path| {
                    let dir = root.join(&path);
                    (path, scope.spawn(move || repo_status(&dir)))
                })
                .collect();
            handles
                .into_iter()
                .map(|(path, handle)| (path, handle.join().ok().flatten()))
                .collect::<BTreeMap<String, Option<RepoStatus>>>()
        })
    })
    .await;
    match statuses {
        Ok(statuses) => Json(statuses).into_response(),
        Err(e) => err_response(anyhow::anyhow!("repo status task panicked: {e}")),
    }
}

/// `git fetch` as the workspace's owner. Answers 200 either way; `ok` says
/// whether git succeeded, `output` what it printed.
pub(crate) async fn post_repo_fetch(
    AxumPath(path): AxumPath<String>,
    WorkspaceExtractor(state): WorkspaceExtractor,
) -> Response {
    match checkouts(&state).await {
        Ok(paths) if paths.contains(&path) => {}
        Ok(_) => return bad_request(format!("{path} is not a downloaded repo of this workspace")),
        Err(e) => return err_response(e),
    }
    let owner = persistence::workspace_owner(state.db.clone(), &state.path).await;

    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(state.path.join(&path))
        .args(["fetch", "--quiet"]);
    if let Some(owner) = &owner {
        owner.apply_to_command(&mut cmd);
    }
    persistence::harden_git_ssh(&mut cmd);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true);

    let (ok, output) = match tokio::time::timeout(FETCH_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            (out.status.success(), text)
        }
        Ok(Err(e)) => (false, format!("failed to run git: {e}")),
        Err(_) => (
            false,
            format!("git fetch took longer than {}s", FETCH_TIMEOUT.as_secs()),
        ),
    };
    let hint = if ok {
        None
    } else {
        downloads::refusal_hint(&output, owner.as_ref())
    };
    Json(serde_json::json!({ "ok": ok, "output": output, "hint": hint })).into_response()
}
