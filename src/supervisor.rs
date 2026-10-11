//! Supervision for detached background tasks. Without it, a panic is parked in
//! an unread `JoinHandle` and an early return goes unnoticed; both now reach
//! [`daemon_log`]. Callers get an [`AbortHandle`] instead.
//!
//! Why `[profile.release]` keeps `panic = "unwind"`: per-task catching needs
//! it, or one panicking workspace takes down the whole daemon.

use std::any::Any;
use std::borrow::Cow;
use std::fmt::Display;
use std::future::Future;

use tokio::task::AbortHandle;

use crate::daemon_log;

pub trait Outcome {
    fn failure(self) -> Option<String>;
}

impl Outcome for () {
    fn failure(self) -> Option<String> {
        None
    }
}

impl<T, E: Display> Outcome for Result<T, E> {
    fn failure(self) -> Option<String> {
        self.err().map(|e| e.to_string())
    }
}

/// For tasks expected to finish. Panics and `Err` are logged.
pub fn supervise<F>(name: impl Into<Cow<'static, str>>, fut: F) -> AbortHandle
where
    F: Future + Send + 'static,
    F::Output: Outcome + Send + 'static,
{
    spawn_watched(name.into(), fut, false)
}

/// For tasks that should run until aborted: returning at all is logged.
pub fn supervise_forever<F>(name: impl Into<Cow<'static, str>>, fut: F) -> AbortHandle
where
    F: Future + Send + 'static,
    F::Output: Outcome + Send + 'static,
{
    spawn_watched(name.into(), fut, true)
}

fn spawn_watched<F>(name: Cow<'static, str>, fut: F, forever: bool) -> AbortHandle
where
    F: Future + Send + 'static,
    F::Output: Outcome + Send + 'static,
{
    let handle = tokio::spawn(fut);
    let abort = handle.abort_handle();

    tokio::spawn(async move {
        match handle.await {
            // `deactivate` aborts deliberately; that's not a fault.
            Err(e) if e.is_cancelled() => {}
            Err(e) => daemon_log::warn(format!(
                "background task `{name}` panicked: {}",
                panic_message(e.into_panic())
            )),
            Ok(out) => match out.failure() {
                Some(why) => daemon_log::warn(format!(
                    "background task `{name}` exited with an error: {why}"
                )),
                None if forever => daemon_log::warn(format!(
                    "background task `{name}` exited on its own; whatever it was serving is no longer being served"
                )),
                None => {}
            },
        }
    });

    abort
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    async fn wait_until(cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never became true");
    }

    fn logged() -> Vec<String> {
        daemon_log::tail(None, 200)
            .into_iter()
            .map(|e| e.message)
            .collect()
    }

    /// A block containing only `panic!` has type `!`, which no `Outcome` covers.
    async fn panics() {
        panic!("boom in a detached task");
    }

    #[tokio::test]
    async fn a_panicking_task_is_reported_instead_of_vanishing() {
        supervise("unit-panic-probe", panics());

        wait_until(|| logged().iter().any(|m| m.contains("unit-panic-probe"))).await;

        let line = logged()
            .into_iter()
            .find(|m| m.contains("unit-panic-probe"))
            .unwrap();
        assert!(line.contains("panicked"), "{line}");
        assert!(line.contains("boom in a detached task"), "{line}");
    }

    #[tokio::test]
    async fn an_error_return_is_reported() {
        supervise("unit-err-probe", async {
            Err::<(), _>(anyhow::anyhow!("the socket went away"))
        });

        wait_until(|| logged().iter().any(|m| m.contains("unit-err-probe"))).await;

        let line = logged()
            .into_iter()
            .find(|m| m.contains("unit-err-probe"))
            .unwrap();
        assert!(line.contains("the socket went away"), "{line}");
    }

    #[tokio::test]
    async fn a_forever_task_returning_cleanly_is_still_reported() {
        supervise_forever("unit-forever-probe", async {});

        wait_until(|| logged().iter().any(|m| m.contains("unit-forever-probe"))).await;

        let line = logged()
            .into_iter()
            .find(|m| m.contains("unit-forever-probe"))
            .unwrap();
        assert!(line.contains("exited on its own"), "{line}");
    }

    #[tokio::test]
    async fn a_normal_completion_is_silent() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        supervise("unit-quiet-probe", async move {
            flag.store(true, Ordering::SeqCst);
        });

        wait_until(|| done.load(Ordering::SeqCst)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!logged().iter().any(|m| m.contains("unit-quiet-probe")));
    }

    #[tokio::test]
    async fn a_deliberate_abort_is_silent() {
        let abort = supervise_forever("unit-abort-probe", async {
            std::future::pending::<()>().await;
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        abort.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!logged().iter().any(|m| m.contains("unit-abort-probe")));
    }
}
