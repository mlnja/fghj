//! Supervision for detached background tasks.
//!
//! `fghjd` spawns ~26 long-lived tasks that nobody ever `.await`s: the DNS
//! server, the two proxy listeners, the three fanned-in effects, each
//! workspace's actor loop, each converge step. Their `JoinHandle`s are kept
//! only so `DaemonControl::deactivate` can `.abort()` them and free the
//! ports.
//!
//! That left two failure modes invisible. A task that **panics** is caught
//! by tokio at the task boundary and the error parked in the `JoinHandle` —
//! which nothing reads, so the panic is never printed anywhere. A task that
//! **returns early** — a `serve` loop breaking out, an effect's channel
//! closing — simply stops, and the daemon carries on reporting itself
//! healthy while its DNS server is gone. Both look identical from the
//! outside to "running normally".
//!
//! [`supervise`] and [`supervise_forever`] close that gap by keeping the
//! `JoinHandle` here, in a watcher task that awaits it, and handing the
//! caller back an [`AbortHandle`] instead. Aborting still works and still
//! frees the port; the difference is that every other way the task can end
//! now reaches [`daemon_log`], and so the Logs tab.
//!
//! This is why `[profile.release]` sets `panic = "unwind"` rather than
//! taking the ~2.3 MB that `panic = "abort"` would save: catching a panic
//! per-task is only possible while unwinding is on. Under `abort`, one
//! panicking workspace would take down DNS, 80/443 and every other
//! workspace with it.

use std::any::Any;
use std::borrow::Cow;
use std::fmt::Display;
use std::future::Future;

use tokio::task::AbortHandle;

use crate::daemon_log;

/// How a supervised task's return value should be read.
///
/// Implemented for `()` (a task that can only end one way) and for
/// `Result<_, E>` (a task that can end badly and, today, has no other way to
/// say so). This is what lets one wrapper cover both without every call site
/// having to adapt its future's output type.
pub trait Outcome {
    /// `None` if the task finished successfully, `Some(reason)` if it
    /// failed.
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

/// Spawns `fut` under supervision. Use for tasks that are *expected* to
/// finish — a converge step, a log capture that ends with its container.
/// Panics and `Err` returns are logged; a clean return is silent.
///
/// The returned [`AbortHandle`] replaces the `JoinHandle` the caller used to
/// get. It aborts the same task the same way; it just can't be awaited,
/// because this module is already awaiting it.
pub fn supervise<F>(name: impl Into<Cow<'static, str>>, fut: F) -> AbortHandle
where
    F: Future + Send + 'static,
    F::Output: Outcome + Send + 'static,
{
    spawn_watched(name.into(), fut, false)
}

/// Spawns `fut` under supervision, for a task that should run until it is
/// aborted. Identical to [`supervise`] except that returning *at all* is
/// treated as a fault and logged, because for these tasks it is one.
///
/// This is the half that catches the quiet failures: `dns::serve` returning
/// `()` because its socket closed is, from every other vantage point in the
/// daemon, indistinguishable from it still serving.
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
            // `abort()` is how `deactivate` is *supposed* to stop these.
            // Staying quiet here is what keeps the distinction between a
            // deliberate shutdown and a task that died on its own.
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

/// Recovers the `panic!` argument. Panics carry a `&str` payload when the
/// message was a literal and a `String` when it was formatted, and neither
/// is guaranteed — hence the fallback rather than an `unwrap`.
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

    /// Polls until `cond` holds, so a test never depends on a fixed sleep
    /// being long enough for the watcher task to be scheduled.
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

    /// Spelled as a named `async fn` rather than an inline block so its
    /// output type is `()`; a block whose body is only `panic!` has type
    /// `!`, which matches neither `Outcome` impl.
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

    /// The whole point of the `forever` variant: this task "succeeded", and
    /// that is precisely the bug.
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

    /// A task that finishes normally is the common case and must not add
    /// noise to the operator's log.
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

    /// `deactivate` aborts these deliberately; that must not look like a
    /// fault in the log.
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
