//! //! Volume discovery: reports what Docker has for the workspace. A volume
//! //! deleted by hand doesn't show as missing — only presence is reported.

use crate::action::Action;
use crate::actor::ActorHandle;
use crate::orchestrator::Orchestrator;

pub async fn observe_volumes(orchestrator: &Orchestrator, actor: &ActorHandle) {
    let names = orchestrator.volume_names().await;
    dispatch_observed_volumes(actor, names).await;
}

async fn dispatch_observed_volumes(actor: &ActorHandle, names: Vec<String>) {
    for name in names {
        let _ = actor
            .dispatch(Action::VolumeObserved {
                volume_name: name,
                exists: true,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WorkspaceState;

    #[tokio::test]
    async fn dispatch_observed_volumes_reports_every_discovered_name() {
        let handle = crate::actor::spawn(crate::state::testing::workspace(vec![]));

        dispatch_observed_volumes(&handle, vec!["pgdata".into(), "cache".into()]).await;

        let current = handle.current();
        let volumes = &current.env.as_ref().unwrap().volumes;
        assert!(volumes["pgdata"].observed.exists);
        assert!(volumes["cache"].observed.exists);
    }

    #[tokio::test]
    async fn dispatch_observed_volumes_with_no_environment_is_a_silent_no_op() {
        let handle = crate::actor::spawn(WorkspaceState::default());

        dispatch_observed_volumes(&handle, vec!["pgdata".into()]).await;

        assert!(handle.current().env.is_none());
    }
}
