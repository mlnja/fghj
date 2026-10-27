//! What fghjd is doing in the background, for every workspace at once: pulls,
//! removals, flow switches and node starts and stops. In memory only; a job's
//! own work has to be safe to run again, so a restart loses the record, never
//! the consistency.
//!
//! The log records work, it doesn't drive it. Pulls and removals run as jobs
//! here; switches and node actions run in the workspace's actor, and the
//! converge effect only reports them here.
//!
//! A job can have a parent. Work it spawns as it goes (each clone of a pull,
//! each node a switch starts or stops) is a child, so the UI can show the
//! tree.

use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::util::time::now_ms;

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Pull,
    Remove,
    /// To a flow; the key is the flow.
    Switch,
    /// One node, keyed by its id, with whatever it brings along as children.
    Start,
    Stop,
    Delete,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    /// A step its job will get to: listed so the plan shows before it runs.
    Queued,
    Running,
    Done,
    Error,
    /// Called off, or its parent finished without it.
    Cancelled,
}

impl ActivityStatus {
    /// Not settled yet.
    fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct Activity {
    pub id: u64,
    /// `None` for daemon-wide work.
    pub workspace_id: Option<String>,
    pub parent: Option<u64>,
    pub kind: ActivityKind,
    /// Names the job within its workspace and kind: `pull-all`, `node:<id>`,
    /// a flow, a node id, a clone's path. A second start of a running one
    /// joins it instead.
    pub key: String,
    pub status: ActivityStatus,
    pub log: String,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

#[derive(Default)]
pub struct ActivityLog {
    inner: Mutex<Inner>,
}

/// An id no job has: ids start at 1.
const DETACHED: u64 = 0;

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// In start order.
    items: Vec<Activity>,
}

impl Inner {
    fn find_mut(&mut self, id: u64) -> Option<&mut Activity> {
        self.items.iter_mut().find(|a| a.id == id)
    }

    fn push(
        &mut self,
        workspace_id: Option<String>,
        parent: Option<u64>,
        kind: ActivityKind,
        key: String,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.items.push(Activity {
            id,
            workspace_id,
            parent,
            kind,
            key,
            status: ActivityStatus::Running,
            log: String::new(),
            started_ms: now_ms(),
            finished_ms: None,
        });
        id
    }

    /// `root` and everything under it.
    fn tree(&self, root: u64) -> Vec<u64> {
        let mut tree = vec![root];
        let mut i = 0;
        while i < tree.len() {
            let id = tree[i];
            tree.extend(
                self.items
                    .iter()
                    .filter(|a| a.parent == Some(id))
                    .map(|a| a.id),
            );
            i += 1;
        }
        tree
    }

    fn remove_tree(&mut self, root: u64) {
        let doomed = self.tree(root);
        self.items.retain(|a| !doomed.contains(&a.id));
    }

    fn find_top(
        &self,
        workspace_id: Option<&str>,
        kind: ActivityKind,
        key: &str,
    ) -> Option<&Activity> {
        self.items.iter().find(|a| {
            a.parent.is_none()
                && a.workspace_id.as_deref() == workspace_id
                && a.kind == kind
                && a.key == key
        })
    }
}

impl ActivityLog {
    /// Starts a top-level job, or, while one with the same workspace, kind and
    /// key is running, hands back that one's snapshot as `Err`. A finished
    /// one is replaced, children and all: each keeps one record.
    pub fn start(
        self: &Arc<Self>,
        workspace_id: Option<&str>,
        kind: ActivityKind,
        key: &str,
    ) -> Result<ActivityHandle, Box<Activity>> {
        let mut inner = self.inner.lock().unwrap();
        let previous = inner.find_top(workspace_id, kind, key).cloned();
        if let Some(previous) = previous {
            if previous.status == ActivityStatus::Running {
                return Err(Box::new(previous));
            }
            inner.remove_tree(previous.id);
        }
        let id = inner.push(
            workspace_id.map(str::to_string),
            None,
            kind,
            key.to_string(),
        );
        Ok(ActivityHandle {
            log: self.clone(),
            id,
        })
    }

    /// Like `start`, but one still running is cancelled rather than joined:
    /// for work whose caller knows the old job is dead (an aborted task whose
    /// guard hasn't dropped yet).
    pub fn start_superseding(
        self: &Arc<Self>,
        workspace_id: Option<&str>,
        kind: ActivityKind,
        key: &str,
    ) -> ActivityHandle {
        match self.start(workspace_id, kind, key) {
            Ok(job) => job,
            Err(running) => {
                ActivityHandle {
                    log: self.clone(),
                    id: running.id,
                }
                .cancel("superseded");
                self.start(workspace_id, kind, key)
                    .expect("cancelled just now, under no other lock")
            }
        }
    }

    /// The top-level `kind` job `key` of `workspace_id`.
    pub fn get(
        &self,
        workspace_id: Option<&str>,
        kind: ActivityKind,
        key: &str,
    ) -> Option<Activity> {
        self.inner
            .lock()
            .unwrap()
            .find_top(workspace_id, kind, key)
            .cloned()
    }

    /// Newest first; only `workspace_id`'s when given.
    pub fn list(&self, workspace_id: Option<&str>) -> Vec<Activity> {
        self.inner
            .lock()
            .unwrap()
            .items
            .iter()
            .rev()
            .filter(|a| workspace_id.is_none() || a.workspace_id.as_deref() == workspace_id)
            .cloned()
            .collect()
    }
}

/// A running job's side of the log: the only way to write to it.
#[derive(Clone)]
pub struct ActivityHandle {
    log: Arc<ActivityLog>,
    id: u64,
}

impl std::fmt::Debug for ActivityHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityHandle")
            .field("id", &self.id)
            .finish()
    }
}

impl ActivityHandle {
    /// Also onto every ancestor's log, so a parent's log reads as the whole
    /// run's transcript and a child's as its own part of it.
    pub fn append(&self, text: &str) {
        let mut inner = self.log.inner.lock().unwrap();
        let mut next = Some(self.id);
        while let Some(id) = next {
            let Some(a) = inner.find_mut(id) else { break };
            a.log.push_str(text);
            next = a.parent;
        }
    }

    pub fn log_len(&self) -> usize {
        self.with(|a| a.log.len()).unwrap_or_default()
    }

    pub fn log_from(&self, start: usize) -> String {
        self.with(|a| a.log.get(start..).unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    /// `None` once the job was replaced.
    pub fn status(&self) -> Option<ActivityStatus> {
        self.with(|a| a.status)
    }

    /// Only for a handle its caller just started: a job replaced since is gone.
    pub fn snapshot(&self) -> Activity {
        self.with(Activity::clone)
            .expect("a handle's activity outlives it")
    }

    /// A job under this one, in the same workspace and of the same kind.
    pub fn child(&self, key: &str) -> ActivityHandle {
        self.child_of_kind(None, key)
    }

    /// A child that waits, `Queued`, until `begin`.
    pub fn queued_child(&self, key: &str) -> ActivityHandle {
        self.queued_child_of_kind(None, key)
    }

    pub fn queued_child_of_kind(&self, kind: Option<ActivityKind>, key: &str) -> ActivityHandle {
        let child = self.child_of_kind(kind, key);
        if let Some(a) = self.log.inner.lock().unwrap().find_mut(child.id) {
            a.status = ActivityStatus::Queued;
        }
        child
    }

    /// Starts a queued job; its clock starts now.
    pub fn begin(&self) {
        if let Some(a) = self.log.inner.lock().unwrap().find_mut(self.id)
            && a.status == ActivityStatus::Queued
        {
            a.status = ActivityStatus::Running;
            a.started_ms = now_ms();
        }
    }

    /// Runs `work` as this queued step: begins it, then settles it with the
    /// result, which it hands back.
    pub async fn step(&self, work: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
        self.begin();
        let result = work.await;
        self.settle(&result);
        result
    }

    /// A child of another kind: a node a switch starts is a `start`.
    pub fn child_of_kind(&self, kind: Option<ActivityKind>, key: &str) -> ActivityHandle {
        let mut inner = self.log.inner.lock().unwrap();
        let Some((workspace_id, own_kind)) = inner
            .find_mut(self.id)
            .map(|a| (a.workspace_id.clone(), a.kind))
        else {
            // Replaced since (a superseded switch still reporting): its
            // children go nowhere too.
            return ActivityHandle {
                log: self.log.clone(),
                id: DETACHED,
            };
        };
        let id = inner.push(
            workspace_id,
            Some(self.id),
            kind.unwrap_or(own_kind),
            key.to_string(),
        );
        ActivityHandle {
            log: self.log.clone(),
            id,
        }
    }

    /// The error goes on this job's log only: a parent that fails because of
    /// it says so in its own `settle`. Children still running are cancelled:
    /// nothing will report on them now.
    pub fn settle(&self, result: &anyhow::Result<()>) {
        match result {
            Ok(()) => self.finish(ActivityStatus::Done, None),
            Err(e) => self.finish(ActivityStatus::Error, Some(format!("\nerror: {e:#}\n"))),
        }
    }

    /// Settles as cancelled, saying `why`, if nothing settled it yet.
    pub fn cancel(&self, why: &str) {
        self.finish(ActivityStatus::Cancelled, Some(format!("\n{why}\n")));
    }

    fn finish(&self, status: ActivityStatus, note: Option<String>) {
        let mut inner = self.log.inner.lock().unwrap();
        let now = now_ms();
        for id in inner.tree(self.id) {
            let Some(a) = inner.find_mut(id) else {
                continue;
            };
            if !a.status.is_open() {
                continue;
            }
            if id == self.id {
                if let Some(note) = &note {
                    a.log.push_str(note);
                }
                a.status = status;
            } else {
                a.status = ActivityStatus::Cancelled;
            }
            a.finished_ms = Some(now);
        }
    }

    /// Cancels the job when dropped unless something settled it first: for
    /// work an abort can drop mid-way.
    pub fn settle_on_drop(self, why: &'static str) -> ActivityGuard {
        ActivityGuard { handle: self, why }
    }

    /// `None` once the job was replaced: a handle outlives its job when a
    /// newer one supersedes it, and its writes then go nowhere.
    fn with<T>(&self, f: impl FnOnce(&Activity) -> T) -> Option<T> {
        let inner = self.log.inner.lock().unwrap();
        inner.items.iter().find(|a| a.id == self.id).map(f)
    }
}

pub struct ActivityGuard {
    handle: ActivityHandle,
    why: &'static str,
}

impl std::ops::Deref for ActivityGuard {
    type Target = ActivityHandle;

    fn deref(&self) -> &ActivityHandle {
        &self.handle
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.handle.cancel(self.why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Arc<ActivityLog> {
        Arc::new(ActivityLog::default())
    }

    #[test]
    fn a_running_key_is_joined_not_started_twice() {
        let log = log();
        let first = log
            .start(Some("ws"), ActivityKind::Pull, "pull-all")
            .unwrap();
        let joined = log
            .start(Some("ws"), ActivityKind::Pull, "pull-all")
            .unwrap_err();
        assert_eq!(joined.id, first.snapshot().id);
    }

    #[test]
    fn the_same_key_in_another_workspace_is_its_own_job() {
        let log = log();
        log.start(Some("a"), ActivityKind::Pull, "pull-all")
            .unwrap();
        assert!(log.start(Some("b"), ActivityKind::Pull, "pull-all").is_ok());
    }

    #[test]
    fn restarting_a_finished_key_replaces_it_and_its_children() {
        let log = log();
        let first = log
            .start(Some("ws"), ActivityKind::Pull, "pull-all")
            .unwrap();
        first.child("api").settle(&Ok(()));
        first.settle(&Ok(()));
        let second = log
            .start(Some("ws"), ActivityKind::Pull, "pull-all")
            .unwrap();
        let all = log.list(Some("ws"));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, second.snapshot().id);
    }

    #[test]
    fn a_childs_output_is_also_its_parents_but_its_error_is_not() {
        let log = log();
        let parent = log
            .start(Some("ws"), ActivityKind::Pull, "pull-all")
            .unwrap();
        let child = parent.child("api");
        child.append("cloning\n");
        child.settle(&Err(anyhow::anyhow!("denied")));
        let child = child.snapshot();
        assert_eq!(child.parent, Some(parent.snapshot().id));
        assert_eq!(child.status, ActivityStatus::Error);
        assert!(child.log.contains("error: denied"));
        assert_eq!(parent.snapshot().log, "cloning\n");
    }

    #[test]
    fn the_same_key_of_another_kind_is_its_own_job() {
        let log = log();
        log.start(Some("ws"), ActivityKind::Start, "api").unwrap();
        assert!(log.start(Some("ws"), ActivityKind::Stop, "api").is_ok());
    }

    #[test]
    fn settling_a_parent_cancels_children_nobody_settled() {
        let log = log();
        let parent = log.start(Some("ws"), ActivityKind::Switch, "app").unwrap();
        let done = parent.child_of_kind(Some(ActivityKind::Start), "db");
        done.settle(&Ok(()));
        let open = parent.child_of_kind(Some(ActivityKind::Start), "api");
        parent.settle(&Err(anyhow::anyhow!("db never got healthy")));
        assert_eq!(done.snapshot().status, ActivityStatus::Done);
        assert_eq!(open.snapshot().status, ActivityStatus::Cancelled);
        assert_eq!(open.snapshot().kind, ActivityKind::Start);
        assert_eq!(parent.snapshot().status, ActivityStatus::Error);
    }

    #[test]
    fn a_dropped_guard_cancels_only_what_is_still_running() {
        let log = log();
        let settled = log.start(Some("ws"), ActivityKind::Start, "a").unwrap();
        {
            let guard = settled.clone().settle_on_drop("called off");
            guard.settle(&Ok(()));
        }
        assert_eq!(settled.snapshot().status, ActivityStatus::Done);

        let aborted = log.start(Some("ws"), ActivityKind::Start, "b").unwrap();
        drop(aborted.clone().settle_on_drop("called off"));
        let aborted = aborted.snapshot();
        assert_eq!(aborted.status, ActivityStatus::Cancelled);
        assert!(aborted.log.contains("called off"));
    }

    #[test]
    fn superseding_cancels_the_running_one_and_starts_afresh() {
        let log = log();
        let stale = log.start(Some("ws"), ActivityKind::Start, "api").unwrap();
        let stale_id = stale.snapshot().id;
        let fresh = log.start_superseding(Some("ws"), ActivityKind::Start, "api");
        assert_ne!(fresh.snapshot().id, stale_id);
        assert_eq!(fresh.snapshot().status, ActivityStatus::Running);

        // The stale task keeps reporting; it reaches nothing and doesn't panic.
        let late = stale.child_of_kind(Some(ActivityKind::Stop), "old");
        late.append("late\n");
        stale.append("late\n");
        late.settle(&Ok(()));
        stale.cancel("aborted");
        assert_eq!(stale.log_len(), 0);
        let all = log.list(Some("ws"));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, ActivityStatus::Running);
        assert!(!all[0].log.contains("late"));
    }

    #[tokio::test]
    async fn queued_steps_wait_run_in_turn_and_are_cancelled_after_a_failure() {
        let log = log();
        let job = log
            .start(Some("ws"), ActivityKind::Remove, "remove")
            .unwrap();
        let [one, two, three] = ["one", "two", "three"].map(|k| job.queued_child(k));
        assert_eq!(two.snapshot().status, ActivityStatus::Queued);

        one.step(async { Ok(()) }).await.unwrap();
        let failed = two.step(async { anyhow::bail!("stuck") }).await;
        job.settle(&failed);

        assert_eq!(one.snapshot().status, ActivityStatus::Done);
        assert_eq!(two.snapshot().status, ActivityStatus::Error);
        assert_eq!(three.snapshot().status, ActivityStatus::Cancelled);
        assert_eq!(job.snapshot().status, ActivityStatus::Error);
    }

    #[test]
    fn list_is_newest_first_and_scoped() {
        let log = log();
        log.start(Some("a"), ActivityKind::Pull, "one").unwrap();
        log.start(Some("b"), ActivityKind::Pull, "two").unwrap();
        log.start(Some("a"), ActivityKind::Pull, "three").unwrap();
        let keys: Vec<String> = log.list(Some("a")).into_iter().map(|a| a.key).collect();
        assert_eq!(keys, ["three", "one"]);
        assert_eq!(log.list(None).len(), 3);
    }
}
