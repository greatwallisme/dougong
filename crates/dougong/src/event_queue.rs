// Rust guideline compliant 2026-10-24
//! Bounded event-listener scheduling with per-event concurrency lanes.
//!
//! Reliable synchronous emits use an unbounded lane to preserve the legacy
//! non-blocking, lossless API. Bounded lanes are opt-in through drop policies
//! or [`Context::emit_with_policy`](crate::Context::emit_with_policy); their
//! capacity counts pending listener invocations, not running ones. A fixed
//! worker pool replaces per-listener task creation, and ready event names are
//! scheduled FIFO so a saturated event cannot block other event names.

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::{Arc, OnceLock, Weak};
use std::task::Poll;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;

use crate::context::{Context, RootState};
use crate::events::{DispatchEntry, run_fire_and_forget};
use crate::fiber::Fiber;
use crate::scope::ScopeNode;

/// Four workers bound task fan-out without assuming a particular host CPU.
const DEFAULT_WORKERS: usize = 4;

/// Construction-fixed event queue and listener-concurrency policies.
#[derive(Debug, Clone)]
pub struct EventQueueConfig {
    /// Maximum concurrent fire-and-forget invocations across queued events.
    pub workers: NonZeroUsize,
    /// Capacity of bounded pending-listener lanes; `None` means unbounded.
    pub queue_capacity: Option<NonZeroUsize>,
    /// Per-event overflow and concurrency policies, keyed by exact event name.
    pub policies: HashMap<String, EventPolicy>,
}

impl Default for EventQueueConfig {
    fn default() -> Self {
        Self {
            workers: NonZeroUsize::new(DEFAULT_WORKERS).unwrap_or(NonZeroUsize::MIN),
            queue_capacity: None,
            policies: HashMap::new(),
        }
    }
}

/// Overflow and concurrency policy for one event name.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventPolicy {
    /// Per-event concurrent-listener cap; `None` leaves only the worker cap.
    pub max_concurrency: Option<NonZeroUsize>,
    /// Behavior when a bounded pending-listener lane is full.
    pub overflow: OverflowPolicy,
}

/// Behavior when a bounded event lane has no pending-slot capacity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Wait asynchronously for capacity; legacy synchronous emit stays lossless.
    #[default]
    Backpressure,
    /// Drop the invocation immediately when the bounded lane is full.
    Drop,
}

/// Result of enqueueing an event through the policy-aware async API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitOutcome {
    /// Every matched listener invocation was accepted for execution.
    Queued,
    /// At least one matched listener invocation was dropped by a full lane.
    Dropped,
}

/// Owns the worker startup gate and queue state for one root context.
pub struct EventDispatcher {
    queue: Arc<Queue>,
    started: OnceLock<()>,
}

impl std::fmt::Debug for EventDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventDispatcher").finish_non_exhaustive()
    }
}

impl EventDispatcher {
    pub(crate) fn new(config: EventQueueConfig) -> Self {
        Self {
            queue: Arc::new(Queue::new(config)),
            started: OnceLock::new(),
        }
    }

    /// Enqueues through the synchronous API: only explicit drop policies are
    /// bounded; reliable events retain their lossless unbounded lane.
    pub(crate) fn enqueue_sync(
        &self,
        ctx: &Context,
        name: &str,
        entry: DispatchEntry,
        payload: Value,
    ) -> EmitOutcome {
        self.start();
        let bounded = self.queue.policy(name).overflow == OverflowPolicy::Drop;
        let job = QueuedJob::new(ctx, name, entry, payload, bounded);
        match self.queue.try_enqueue(job) {
            Ok(()) => EmitOutcome::Queued,
            Err(EnqueueError::Full(_) | EnqueueError::Closed) => EmitOutcome::Dropped,
        }
    }

    /// Enqueues through the policy-aware async API. Backpressure waits for a
    /// bounded slot; drop returns immediately when the lane is full.
    pub(crate) async fn enqueue_async(
        &self,
        ctx: &Context,
        name: &str,
        entry: DispatchEntry,
        payload: Value,
    ) -> EmitOutcome {
        self.start();
        let overflow = self.queue.policy(name).overflow;
        let job = QueuedJob::new(ctx, name, entry, payload, true);
        match overflow {
            OverflowPolicy::Drop => match self.queue.try_enqueue(job) {
                Ok(()) => EmitOutcome::Queued,
                Err(EnqueueError::Full(_) | EnqueueError::Closed) => EmitOutcome::Dropped,
            },
            OverflowPolicy::Backpressure => {
                if self.queue.enqueue(job).await {
                    EmitOutcome::Queued
                } else {
                    EmitOutcome::Dropped
                }
            }
        }
    }

    fn start(&self) {
        self.started.get_or_init(|| {
            for _ in 0..self.queue.config.workers.get() {
                let queue = Arc::clone(&self.queue);
                tokio::spawn(async move { worker_loop(queue).await });
            }
        });
    }
}

impl Drop for EventDispatcher {
    fn drop(&mut self) {
        self.queue.close();
    }
}

struct Queue {
    config: EventQueueConfig,
    state: Mutex<QueueState>,
    work: Notify,
    space: Notify,
}

impl Queue {
    fn new(config: EventQueueConfig) -> Self {
        Self {
            config,
            state: Mutex::new(QueueState::default()),
            work: Notify::new(),
            space: Notify::new(),
        }
    }

    fn policy(&self, name: &str) -> EventPolicy {
        self.config.policies.get(name).copied().unwrap_or_default()
    }

    fn try_enqueue(&self, job: QueuedJob) -> Result<(), EnqueueError> {
        let bounded = job.bounded;
        {
            let mut state = self.state.lock();
            if state.closed {
                return Err(EnqueueError::Closed);
            }
            if bounded
                && self
                    .config
                    .queue_capacity
                    .is_some_and(|capacity| state.bounded_pending >= capacity.get())
            {
                return Err(EnqueueError::Full(Box::new(job)));
            }
            let name = job.name.clone();
            let cap = self
                .policy(&name)
                .max_concurrency
                .map_or(usize::MAX, NonZeroUsize::get);
            let ready = {
                let lane = state
                    .lanes
                    .entry(name.clone())
                    .or_insert_with(|| Lane::new(cap));
                lane.pending.push_back(job);
                lane.active < cap && !lane.ready
            };
            if bounded {
                state.bounded_pending += 1;
            }
            if ready {
                if let Some(lane) = state.lanes.get_mut(&name) {
                    lane.ready = true;
                }
                state.ready.push_back(name);
            }
        }
        self.work.notify_one();
        Ok(())
    }

    async fn enqueue(&self, mut job: QueuedJob) -> bool {
        loop {
            let notified = self.space.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_enqueue(job) {
                Ok(()) => return true,
                Err(EnqueueError::Full(returned)) => job = *returned,
                Err(EnqueueError::Closed) => return false,
            }
            notified.await;
        }
    }

    fn pop(&self) -> Option<QueuedJob> {
        let mut state = self.state.lock();
        if state.closed {
            return None;
        }
        let name = state.ready.pop_front()?;
        let (job, ready) = {
            let lane = state.lanes.get_mut(&name)?;
            lane.ready = false;
            let job = lane.pending.pop_front()?;
            lane.active += 1;
            let ready = !lane.pending.is_empty() && lane.active < lane.cap;
            (job, ready)
        };
        if ready {
            if let Some(lane) = state.lanes.get_mut(&name) {
                lane.ready = true;
            }
            state.ready.push_back(name);
        }
        if job.bounded {
            state.bounded_pending -= 1;
        }
        drop(state);
        if job.bounded {
            self.space.notify_one();
        }
        Some(job)
    }

    fn complete(&self, name: &str) {
        let ready = {
            let mut state = self.state.lock();
            let (ready, remove) = {
                let Some(lane) = state.lanes.get_mut(name) else {
                    return;
                };
                lane.active -= 1;
                let ready = lane.active < lane.cap && !lane.pending.is_empty() && !lane.ready;
                if ready {
                    lane.ready = true;
                }
                let remove = lane.active == 0 && lane.pending.is_empty() && !lane.ready;
                (ready, remove)
            };
            if ready {
                state.ready.push_back(name.to_string());
            }
            if remove {
                state.lanes.remove(name);
            }
            ready
        };
        if ready {
            self.work.notify_one();
        }
    }

    fn is_closed(&self) -> bool {
        self.state.lock().closed
    }

    fn close(&self) {
        let lanes = {
            let mut state = self.state.lock();
            state.closed = true;
            state.ready.clear();
            state.bounded_pending = 0;
            std::mem::take(&mut state.lanes)
        };
        drop(lanes);
        self.work.notify_waiters();
        self.space.notify_waiters();
    }
}

#[derive(Default)]
struct QueueState {
    lanes: HashMap<String, Lane>,
    ready: VecDeque<String>,
    bounded_pending: usize,
    closed: bool,
}

struct Lane {
    pending: VecDeque<QueuedJob>,
    active: usize,
    cap: usize,
    ready: bool,
}

impl Lane {
    const fn new(cap: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            active: 0,
            cap,
            ready: false,
        }
    }
}

struct QueuedJob {
    root: Weak<RootState>,
    scope: Arc<ScopeNode>,
    fiber: Weak<Fiber>,
    entry: DispatchEntry,
    name: String,
    payload: Value,
    bounded: bool,
}

impl QueuedJob {
    fn new(ctx: &Context, name: &str, entry: DispatchEntry, payload: Value, bounded: bool) -> Self {
        Self {
            root: Arc::downgrade(&ctx.root),
            scope: Arc::clone(&ctx.scope),
            fiber: Weak::clone(&ctx.fiber),
            entry,
            name: name.to_string(),
            payload,
            bounded,
        }
    }

    async fn run(self) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let ctx = Context {
            root,
            scope: self.scope,
            fiber: self.fiber,
        };
        let budget = ctx.root.timeouts.listener;
        let work = run_fire_and_forget(self.entry.clone(), self.name, ctx, self.payload, budget);
        let _ = self.entry.run_scoped(work).await;
    }
}

enum EnqueueError {
    Full(Box<QueuedJob>),
    Closed,
}

async fn worker_loop(queue: Arc<Queue>) {
    loop {
        let notified = queue.work.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let job = queue.pop();
        let Some(job) = job else {
            if queue.is_closed() {
                return;
            }
            notified.await;
            continue;
        };
        let name = job.name.clone();
        let mut work = Box::pin(job.run());
        std::future::poll_fn(|cx| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.as_mut().poll(cx)))
            {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(())) | Err(_) => Poll::Ready(()),
            }
        })
        .await;
        queue.complete(&name);
    }
}
