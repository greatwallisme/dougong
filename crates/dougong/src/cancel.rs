// Rust guideline compliant 2026-10-24
//! Per-activation cancel scopes (stability track S2, improvement 二.2).
//!
//! Every task a fiber owns — listener invocations dispatched to it and the
//! plugin body itself — is spawned through the fiber's current scope
//! generation. Unloading the activation cancels the scope and reaps it:
//!
//! - **abort-safe** work (in-process futures) is dropped at its next yield
//!   point, which is exactly `tokio::task::abort` granularity;
//! - **tracked** work (boundary bridges, wasm bodies) is never dropped
//!   mid-call — a wasmtime store forbids abandoning an in-flight
//!   `call_async` — so the reap awaits its completion instead (the sandbox
//!   fuel budget bounds that wait).
//!
//! Spawning into a cancelled scope is a deterministic no-op (`None`), so a
//! dispatch that raced an unload never starts work for a dead listener. The
//! unload path reaps *before* draining effects, so no spawned guest call
//! outlives the instance teardown (a dylib post-teardown call is documented
//! UB). Inline dispatch modes (serial/bail/waterfall) run on the emitter's
//! task and are covered by a dead-listener skip at the call site plus the
//! boundary adapters' per-instance locks, not by this scope.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Cancellation scope for one fiber activation generation (see module docs).
pub struct CancelScope {
    /// Set once the owning activation unloads; never re-armed (a reload
    /// mints a fresh generation instead).
    cancelled: AtomicBool,
    /// Broadcast to abort-safe wrappers when the scope is cancelled.
    cancel_signal: Notify,
    /// Live task count; the reaper waits for it to drain to zero.
    tasks: AtomicUsize,
    /// Broadcast whenever the count drains to zero.
    empty_signal: Notify,
}

impl std::fmt::Debug for CancelScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelScope")
            .field("cancelled", &self.is_cancelled())
            .field("tasks", &self.task_count())
            .finish()
    }
}

impl CancelScope {
    /// A live scope with no tasks.
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            cancel_signal: Notify::new(),
            tasks: AtomicUsize::new(0),
            empty_signal: Notify::new(),
        }
    }

    /// True once the scope has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Number of admitted tasks still running (diagnostics and the
    /// invariant layer's reaped check).
    pub fn task_count(&self) -> usize {
        self.tasks.load(Ordering::SeqCst)
    }

    /// Cancels the scope: abort-safe tasks are dropped at their next yield
    /// point and the spawn gate closes. Idempotent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel_signal.notify_waiters();
    }

    /// Cancels the scope and waits until every admitted task has finished.
    ///
    /// Tracked tasks run to completion; a task that never yields (a
    /// synchronous foreign call that never returns) keeps the reap waiting —
    /// that hang class belongs to the timeout policy (S3), not here.
    pub async fn cancel_and_reap(&self) {
        self.cancel();
        loop {
            let notified = self.empty_signal.notified();
            tokio::pin!(notified);
            // Register the waiter before re-checking the count so the
            // drain-to-zero broadcast cannot be missed in between.
            notified.as_mut().enable();
            if self.tasks.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Spawns abort-safe work: once the scope is cancelled the future is
    /// dropped at its next yield point and the handle resolves to `None`.
    /// Returns `None` without spawning when the scope is already cancelled.
    pub fn spawn<F, T>(self: &Arc<Self>, future: F) -> Option<JoinHandle<Option<T>>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        if !self.enter() {
            return None;
        }
        let scope = Arc::clone(self);
        Some(tokio::spawn(async move {
            let guard = TaskGuard(scope);
            let notified = guard.0.cancel_signal.notified();
            tokio::pin!(notified);
            // Register the waiter before checking the flag so a concurrent
            // cancel cannot slip through unobserved.
            notified.as_mut().enable();
            if guard.0.is_cancelled() {
                return None;
            }
            tokio::select! {
                biased;
                () = &mut notified => None,
                result = future => Some(result),
            }
        }))
    }

    /// Spawns tracked-only work: the future is never dropped mid-flight, so
    /// the handle always resolves to `Some` on completion. Cancellation only
    /// closes the spawn gate; the reap awaits the task instead.
    pub fn spawn_tracked<F, T>(self: &Arc<Self>, future: F) -> Option<JoinHandle<Option<T>>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        if !self.enter() {
            return None;
        }
        let scope = Arc::clone(self);
        Some(tokio::spawn(async move {
            let _guard = TaskGuard(scope);
            Some(future.await)
        }))
    }

    /// Admits one task into the scope; `false` when cancelled.
    ///
    /// The increment is sequenced before the flag check and both use
    /// `SeqCst`: a reap that observed a zero count must not be followed by a
    /// late spawn slipping past the gate, and a spawn that observed the flag
    /// must not have been counted by a reap that already returned. That
    /// store-load pairing is the Dekker pattern — weaker orderings leave the
    /// window open.
    fn enter(&self) -> bool {
        self.tasks.fetch_add(1, Ordering::SeqCst);
        if self.cancelled.load(Ordering::SeqCst) {
            self.leave();
            return false;
        }
        true
    }

    /// Releases one task; broadcasts when the count drains to zero.
    fn leave(&self) {
        if self.tasks.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.empty_signal.notify_waiters();
        }
    }
}

/// Decrements the live-task count on every exit path, including panics and
/// external aborts (task teardown runs `Drop` either way).
struct TaskGuard(Arc<CancelScope>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test rail: any hang becomes a failure instead of a stuck suite.
    const RAIL: std::time::Duration = std::time::Duration::from_secs(5);

    /// Records when a dropped future's captured state is released.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn spawned_task_completes_and_drains() {
        let scope = Arc::new(CancelScope::new());
        let handle = scope.spawn(async { 7 }).expect("scope admits the task");
        assert_eq!(handle.await.expect("task joined"), Some(7));
        assert_eq!(scope.task_count(), 0);
    }

    #[tokio::test]
    async fn spawn_into_cancelled_scope_is_rejected() {
        let scope = Arc::new(CancelScope::new());
        scope.cancel();
        assert!(scope.spawn(async {}).is_none());
        assert!(scope.spawn_tracked(async {}).is_none());
        assert_eq!(scope.task_count(), 0);
    }

    #[tokio::test]
    async fn cancel_drops_an_unstarted_future() {
        // No yield between spawn and cancel: on the current-thread runtime
        // the task is first polled *after* cancellation and takes the
        // flag-check fast path, dropping the never-polled future.
        let scope = Arc::new(CancelScope::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let handle = scope
            .spawn(async move {
                let _flag = flag;
                std::future::pending::<()>().await;
            })
            .expect("scope admits the task");
        tokio::time::timeout(RAIL, scope.cancel_and_reap())
            .await
            .expect("reap settles");
        assert!(handle.await.expect("task joined").is_none());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancel_drops_a_running_future_at_its_yield_point() {
        let scope = Arc::new(CancelScope::new());
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let mark = Arc::clone(&started);
        let handle = scope
            .spawn(async move {
                let _flag = flag;
                mark.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            })
            .expect("scope admits the task");
        // Let the task start and park on the pending point, so the abort
        // drops a future that is genuinely in flight.
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(RAIL, scope.cancel_and_reap())
            .await
            .expect("reap settles");
        assert!(handle.await.expect("task joined").is_none());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn tracked_task_runs_to_completion_despite_cancel() {
        let scope = Arc::new(CancelScope::new());
        let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
        let handle = scope
            .spawn_tracked(async move { rx.await.expect("release latch") })
            .expect("scope admits the task");
        scope.cancel();
        tx.send(41).expect("tracked task keeps its receiver");
        assert_eq!(handle.await.expect("task joined"), Some(41));
        tokio::time::timeout(RAIL, scope.cancel_and_reap())
            .await
            .expect("reap settles");
    }

    #[tokio::test]
    async fn reap_waits_for_tracked_task() {
        let scope = Arc::new(CancelScope::new());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = scope
            .spawn_tracked(async move { rx.await.expect("release latch") })
            .expect("scope admits the task");
        let reaper = {
            let scope = Arc::clone(&scope);
            tokio::spawn(async move { scope.cancel_and_reap().await })
        };
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(!reaper.is_finished(), "reap must wait for the tracked task");
        tx.send(()).expect("tracked task keeps its receiver");
        handle.await.expect("task joined");
        tokio::time::timeout(RAIL, reaper)
            .await
            .expect("reap settles")
            .expect("reaper joined");
        assert_eq!(scope.task_count(), 0);
    }

    #[tokio::test]
    async fn reap_with_zero_tasks_returns_and_cancel_is_idempotent() {
        let scope = Arc::new(CancelScope::new());
        tokio::time::timeout(RAIL, scope.cancel_and_reap())
            .await
            .expect("empty reap returns immediately");
        scope.cancel();
        tokio::time::timeout(RAIL, scope.cancel_and_reap())
            .await
            .expect("repeated reap returns");
        assert_eq!(scope.task_count(), 0);
    }

    #[tokio::test]
    async fn panicking_task_still_drains() {
        let scope = Arc::new(CancelScope::new());
        let handle = scope
            .spawn(async move {
                panic!("boom"); // deliberate panic: exercises task-panic accounting
            })
            .expect("scope admits the task");
        let err = handle.await.expect_err("panic surfaces as JoinError");
        assert!(err.is_panic());
        assert_eq!(scope.task_count(), 0);
    }

    #[tokio::test]
    async fn spawn_after_reap_stays_rejected() {
        let scope = Arc::new(CancelScope::new());
        let handle = scope.spawn(async { 3 }).expect("scope admits the task");
        let _ = handle.await;
        scope.cancel_and_reap().await;
        assert!(scope.spawn(async { 4 }).is_none());
        assert!(scope.spawn_tracked(async { 5 }).is_none());
    }
}
