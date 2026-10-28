//! Browsers' own certificate stores, filled by fghjd for every workspace's
//! owner, as that owner: on every start, and when a workspace is wired, so
//! `fghj wire` returns with the browsers already trusting the CA.

use crate::daemon::ca_dir;
use crate::daemon::registry::WorkspaceRegistry;
use crate::persistence::{self, WorkspaceOwner};
use crate::{platform, web};

/// One per user, however many workspaces they own: the owner `fghj wire`
/// recorded, else whoever owns the folder, exactly as `git clone` runs. Not
/// `registry.states()`: the actors' snapshots don't carry the owner.
pub(crate) async fn owners(registry: &WorkspaceRegistry) -> Vec<WorkspaceOwner> {
    let mut owners: Vec<WorkspaceOwner> = Vec::new();
    for (id, _) in registry.list() {
        let Some(state) = registry.get(&id) else {
            continue;
        };
        if let Some(owner) = persistence::workspace_owner(state.db.clone(), &state.path).await
            && !owners.iter().any(|o| o.uid == owner.uid)
        {
            owners.push(owner);
        }
    }
    owners
}

/// What went wrong, one line per owner it failed for.
pub(crate) async fn trust_for(owners: Vec<WorkspaceOwner>) -> Vec<String> {
    let cert = web::ca::ca_cert_path(&ca_dir());
    tokio::task::spawn_blocking(move || {
        owners
            .iter()
            .filter_map(|owner| {
                platform::current()
                    .browser_trust
                    .install(owner, &cert)
                    .err()
                    .map(|e| format!("{e:#}"))
            })
            .collect()
    })
    .await
    .unwrap_or_else(|e| vec![format!("the browser trust task panicked: {e}")])
}
