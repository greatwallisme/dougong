// Rust guideline compliant 2026-10-24
//! Event bus with the five cordis dispatch modes.
//!
//! Events are named (`&str`) and carry one JSON payload. Every listener is an
//! effect of the fiber that registered it, so unloading the fiber removes the
//! listener (TS `EventsService::register`). Dispatch modes mirror the paper's
//! contract and TS `events.ts`:
//!
//! - `emit`: fire-and-forget; listener failures are logged, never propagated.
//! - `parallel`: await all listeners; aggregate failures into one error.
//! - `serial`: await listeners in registration order; stop at first bail.
//! - `bail`: first-bail-wins (async twins of cordis' sync `bail`).
//! - `waterfall`: each listener wraps a `next` continuation; skipping `next`
//!   vetoes the rest of the chain including the fallback.
//!
//! `internal/*` events are framework extension points; `internal/dispatch`
//! fires before every non-internal dispatch.
//!
//! S5: every listener invocation that settles as a failure additionally runs
//! the per-class reaction of [`crate::ErrorPolicies`] (log level plus an
//! optional restart/dispose of the offending fiber); the mode's propagation
//! semantics are unchanged.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use serde_json::Value;

use crate::cancel::CancelScope;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::fiber::{EffectGuard, Fiber};
use crate::plugin::BoxFuture;

/// A listener callback: receives the dispatch context, payload, and a
/// continuation used only by waterfall dispatch.
pub type Listener = Arc<dyn Fn(Context, Value, Next) -> BoxFuture<Result<Value>> + Send + Sync>;

/// The dispatch mode a listener invocation runs under (decision D8: modes
/// live at dispatch, never at registration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Fire-and-forget.
    Emit,
    /// Concurrent, all awaited.
    Parallel,
    /// In order, first bail stops.
    Serial,
    /// First bail wins.
    Bail,
    /// Continuation chaining.
    Waterfall,
}

impl DispatchMode {
    /// The stable lowercase name used in `internal/dispatch` payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Emit => "emit",
            Self::Parallel => "parallel",
            Self::Serial => "serial",
            Self::Bail => "bail",
            Self::Waterfall => "waterfall",
        }
    }
}

/// A listener that also observes the dispatch mode; boundary bridges use it
/// to forward the mode across the plugin boundary.
pub type DispatchListener =
    Arc<dyn Fn(Context, Value, Next, DispatchMode) -> BoxFuture<Result<Value>> + Send + Sync>;

/// The continuation passed to waterfall listeners. Calling it advances the
/// chain; dropping it without calling vetoes the rest of the dispatch.
#[derive(Clone)]
pub struct Next {
    inner: Arc<dyn Fn(Value) -> BoxFuture<Result<Value>> + Send + Sync>,
}

impl std::fmt::Debug for Next {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Next").finish_non_exhaustive()
    }
}

impl Next {
    pub(crate) fn new(inner: Arc<dyn Fn(Value) -> BoxFuture<Result<Value>> + Send + Sync>) -> Self {
        Self { inner }
    }

    /// Advances the chain with `payload`.
    pub fn call(&self, payload: Value) -> BoxFuture<Result<Value>> {
        (self.inner)(payload)
    }
}

/// True when a `serial`/`bail` result stops the chain. Mirrors cordis
/// `isBailed`: every non-null value bails.
#[must_use]
pub fn is_bailed(value: &Value) -> bool {
    !value.is_null()
}

#[derive(Clone)]
struct Hook {
    ctx: Context,
    callback: Listener,
    global: bool,
    /// Mode-aware bridge; absent for listeners registered through `on`.
    bridge: Option<DispatchListener>,
}

/// One resolvable listener: plain, or a mode-aware bridge.
#[derive(Clone)]
struct DispatchEntry {
    callback: Listener,
    bridge: Option<DispatchListener>,
    /// The owning fiber's cancel-scope generation at snapshot time (S2).
    scope: Option<Arc<CancelScope>>,
    /// The owning fiber itself (weak), for the S5 listener-failure reaction.
    /// The root fiber (uid 0) is framework-owned and never acted on.
    fiber: Option<Weak<Fiber>>,
}

impl DispatchEntry {
    pub(crate) async fn invoke(
        self,
        ctx: Context,
        payload: Value,
        next: Next,
        mode: DispatchMode,
    ) -> Result<Value> {
        match self.bridge {
            Some(bridge) => bridge(ctx, payload, next, mode).await,
            None => (self.callback)(ctx, payload, next).await,
        }
    }

    /// True when the owning fiber is gone or its scope generation was
    /// cancelled: the listener counts as unregistered for this dispatch.
    fn is_dead(&self) -> bool {
        self.scope.as_ref().is_none_or(|scope| scope.is_cancelled())
    }

    /// Spawns the invocation under the owning fiber's scope (S2). Plain
    /// listeners are abort-safe — dropped at their next yield point once
    /// the fiber unloads; boundary bridges are tracked-only — never dropped
    /// mid-call, because a wasm store forbids abandoning an in-flight
    /// `call_async` — so the unload reap awaits them instead. Returns
    /// `None` without spawning when the listener's fiber is gone or its
    /// scope already cancelled (a vanished listener is absent, not failed).
    fn spawn_scoped<F, T>(self, future: F) -> Option<tokio::task::JoinHandle<Option<T>>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let scope = Arc::clone(self.scope.as_ref()?);
        if self.bridge.is_some() {
            scope.spawn_tracked(future)
        } else {
            scope.spawn(future)
        }
    }
}

/// Mode-specific dispatch data shared by the free functions below.
pub struct Dispatch {
    entries: Vec<DispatchEntry>,
}

/// Registry of listeners, keyed by event name.
#[derive(Default)]
pub struct EventStore {
    hooks: HashMap<String, Vec<Hook>>,
}

impl std::fmt::Debug for EventStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&String> = self.hooks.keys().collect();
        names.sort();
        f.debug_struct("EventStore")
            .field("events", &names)
            .finish()
    }
}

impl EventStore {
    pub(crate) fn register(
        &mut self,
        name: &str,
        ctx: Context,
        callback: Listener,
        global: bool,
        bridge: Option<DispatchListener>,
    ) {
        let hooks = self.hooks.entry(name.to_string()).or_default();
        hooks.push(Hook {
            ctx,
            callback,
            global,
            bridge,
        });
    }

    pub(crate) fn remove(&mut self, name: &str, callback: &Listener) -> bool {
        let Some(hooks) = self.hooks.get_mut(name) else {
            return false;
        };
        let before = hooks.len();
        hooks.retain(|hook| !Arc::ptr_eq(&hook.callback, callback));
        let removed = hooks.len() != before;
        if hooks.is_empty() {
            self.hooks.remove(name);
        }
        removed
    }

    /// Snapshot of matching listeners, applying the global/filter rule.
    ///
    /// Without a filter every listener matches (TS dispatches with a null
    /// `thisArg`); with one, hooks flagged `global` always match and the rest
    /// are decided by the filter (service-scoped `internal/service` events
    /// filter by isolation label, as in TS `notify`).
    pub(crate) fn matching(
        &self,
        name: &str,
        filter: Option<&dyn Fn(&Context) -> bool>,
    ) -> Dispatch {
        let hooks = self.hooks.get(name).map_or(&[][..], Vec::as_slice);
        let entries = hooks
            .iter()
            .filter(|hook| filter.is_none_or(|decides| hook.global || decides(&hook.ctx)))
            .map(|hook| DispatchEntry {
                callback: Arc::clone(&hook.callback),
                bridge: hook.bridge.clone(),
                scope: hook.ctx.fiber().map(|fiber| fiber.current_scope()),
                fiber: Some(Weak::clone(&hook.ctx.fiber)),
            })
            .collect();
        Dispatch { entries }
    }
}

/// Fires `internal/dispatch` for observability, then resolves listeners.
fn begin_dispatch(ctx: &Context, mode: &str, name: &str) -> Dispatch {
    if !name.starts_with("internal/") {
        let payload = serde_json::json!({ "mode": mode, "event": name });
        crate::events::emit_internal(ctx, "internal/dispatch", &payload, None);
    }
    ctx.root.events.lock().matching(name, None)
}

/// A null continuation for modes that never advance a chain.
fn null_next() -> Next {
    Next::new(Arc::new(|_| {
        Box::pin(async { Ok(Value::Null) }) as BoxFuture<Result<Value>>
    }))
}

/// How one bounded listener invocation settled (S3).
enum InvokeOutcome {
    /// The listener ran and returned (possibly with an error, including
    /// `listener_timeout`).
    Returned(Result<Value>),
    /// The listener's fiber vanished between the dead-check and the spawn
    /// (bridge path only): the listener counts as absent, never failed. The
    /// payload comes back so the caller can pass it down the chain.
    Vanished(Value),
}

/// How one awaited parallel-dispatch listener task settled (S3).
enum Joined {
    /// The `JoinHandle` resolved (task finished, vanished, or died).
    Done(Result<Option<Result<Value>>, tokio::task::JoinError>),
    /// The await-side budget of a tracked listener fired.
    Elapsed,
}

/// Invokes one dispatch entry under the listener budget (S3).
///
/// A plain listener's future is abort-safe, so its budget is enforced by
/// dropping the future at a poll boundary. A bridge's foreign call is never
/// dropped mid-flight: it runs tracked inside the listener fiber's scope on
/// a *cloned* payload and the dispatcher simply stops awaiting at the budget
/// — the call keeps running there and the unload reap still accounts for it.
async fn invoke_bounded(
    entry: DispatchEntry,
    name: &str,
    ctx: Context,
    payload: Value,
    next: Next,
    mode: DispatchMode,
    budget: std::time::Duration,
) -> InvokeOutcome {
    let fiber = entry.fiber.clone();
    let react_ctx = ctx.clone();
    let outcome = invoke_bounded_inner(entry, name, ctx, payload, next, mode, budget).await;
    // S5: one settled failure = one policy reaction on the offending fiber,
    // regardless of the dispatch mode the invocation ran under.
    if let InvokeOutcome::Returned(Err(err)) = &outcome {
        crate::policy::react_to_listener_failure(&react_ctx, fiber.as_ref(), err);
    }
    outcome
}

/// The budgeted invocation itself, split from [`invoke_bounded`] so the S5
/// reaction wraps every settled-failure arm in one place.
async fn invoke_bounded_inner(
    entry: DispatchEntry,
    name: &str,
    ctx: Context,
    payload: Value,
    next: Next,
    mode: DispatchMode,
    budget: std::time::Duration,
) -> InvokeOutcome {
    if entry.bridge.is_none() {
        let result =
            match tokio::time::timeout(budget, entry.invoke(ctx, payload, next, mode)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(Error::listener_timeout(name)),
            };
        return InvokeOutcome::Returned(result);
    }
    let worker = entry.clone();
    let spawned = entry.spawn_scoped(worker.invoke(ctx, payload.clone(), next, mode));
    let Some(handle) = spawned else {
        return InvokeOutcome::Vanished(payload);
    };
    match tokio::time::timeout(budget, handle).await {
        Ok(Ok(Some(result))) => InvokeOutcome::Returned(result),
        // `spawn_scoped` admits the task before the scope can cancel it, so
        // a `None` here means the listener's fiber unloaded in flight.
        Ok(Ok(None)) => InvokeOutcome::Vanished(payload),
        Ok(Err(join_error)) => InvokeOutcome::Returned(Err(Error::listener_panicked(&join_error))),
        // Dropping the JoinHandle await detaches the task; the tracked
        // foreign call itself is never dropped.
        Err(_elapsed) => InvokeOutcome::Returned(Err(Error::listener_timeout(name))),
    }
}

/// Fire-and-forget listener work under the listener budget (S3): a plain
/// listener's future is dropped at the budget (abort-safe); a bridge call is
/// never dropped mid-flight — the timeout is logged and the task keeps
/// awaiting the foreign call to completion (pausing a `call_async` future is
/// sound; only dropping it is forbidden).
///
/// S5: failures log at the level their class's policy assigns and run the
/// class's listener-failure reaction. One invocation reacts at most once: a
/// bridge that first misses its budget reacts to the timeout; a later
/// completion error of the same call is only logged.
async fn run_fire_and_forget(
    entry: DispatchEntry,
    name: String,
    ctx: Context,
    payload: Value,
    budget: Option<std::time::Duration>,
) {
    let tracked = entry.bridge.is_some();
    let fiber = entry.fiber.clone();
    let invoke = entry.invoke(ctx.clone(), payload, null_next(), DispatchMode::Emit);
    let note = |err: &Error, react: bool| {
        ctx.root.policies.log_error(&*ctx.logger(), err);
        if react {
            crate::policy::react_to_listener_failure(&ctx, fiber.as_ref(), err);
        }
    };
    match (budget, tracked) {
        (Some(budget), false) => match tokio::time::timeout(budget, invoke).await {
            Ok(Err(err)) => note(&err, true),
            Ok(Ok(_)) => {}
            Err(_elapsed) => note(&Error::listener_timeout(&name), true),
        },
        (Some(budget), true) => {
            tokio::pin!(invoke);
            match tokio::time::timeout(budget, &mut invoke).await {
                // A within-budget failure was previously dropped unlogged;
                // S5 closes that swallow gap.
                Ok(Err(err)) => note(&err, true),
                Ok(Ok(_)) => {}
                Err(_elapsed) => {
                    note(&Error::listener_timeout(&name), true);
                    if let Err(err) = invoke.await {
                        note(&err, false);
                    }
                }
            }
        }
        _ => {
            if let Err(err) = invoke.await {
                note(&err, true);
            }
        }
    }
}

/// Fire-and-forget dispatch. Listener failures are logged, never propagated.
pub fn emit(ctx: &Context, name: &str, payload: Value) {
    let ctx = ctx.clone();
    let name = name.to_string();
    // The dispatch task itself stays emitter-independent (C7): an emitter
    // teardown never revokes in-flight delivery to *other* listeners. Each
    // listener task belongs to its owning fiber's cancel scope instead (S2).
    tokio::spawn(async move {
        let budget = ctx.root.timeouts.listener;
        let dispatch = begin_dispatch(&ctx, "emit", &name);
        for entry in dispatch.entries {
            let work = run_fire_and_forget(
                entry.clone(),
                name.clone(),
                ctx.clone(),
                payload.clone(),
                budget,
            );
            // Detached on purpose: fire-and-forget. `None` means the
            // listener's fiber vanished between snapshot and spawn — skipped.
            let _ = entry.spawn_scoped(work);
        }
    });
}

/// Concurrent dispatch; waits for every listener and aggregates failures.
///
/// # Errors
///
/// Returns an aggregate error when any listener fails.
pub async fn parallel(ctx: &Context, name: &str, payload: Value) -> Result<Value> {
    let budget = ctx.root.timeouts.listener;
    let dispatch = begin_dispatch(ctx, "parallel", name);
    let mut tasks = Vec::with_capacity(dispatch.entries.len());
    for entry in dispatch.entries {
        let ctx = ctx.clone();
        let payload = payload.clone();
        let worker = entry.clone();
        // Kept for the S5 failure reaction at join time; the invocation
        // itself consumes `entry` at spawn.
        let reactor = entry.clone();
        let tracked = entry.bridge.is_some();
        let work = async move {
            worker
                .invoke(ctx, payload, null_next(), DispatchMode::Parallel)
                .await
        };
        // A plain listener's budget is enforced inside its own task (the
        // future is abort-safe, so dropping it at the budget is the S2
        // granularity); a bridge's budget is enforced await-side below so
        // the tracked foreign call is never dropped mid-flight.
        let spawned = match budget.filter(|_| !tracked) {
            Some(budget) => {
                let name = name.to_string();
                entry.spawn_scoped(async move {
                    match tokio::time::timeout(budget, work).await {
                        Ok(result) => result,
                        Err(_elapsed) => Err(Error::listener_timeout(&name)),
                    }
                })
            }
            None => entry.spawn_scoped(work),
        };
        // `None`: the listener's fiber unloaded between snapshot and spawn.
        if let Some(handle) = spawned {
            tasks.push((handle, budget.filter(|_| tracked), reactor));
        }
    }
    let mut failures = Vec::new();
    let mut last = payload;
    for (task, await_budget, entry) in tasks {
        let joined = match await_budget {
            Some(budget) => match tokio::time::timeout(budget, task).await {
                Ok(result) => Joined::Done(result),
                Err(_elapsed) => Joined::Elapsed,
            },
            None => Joined::Done(task.await),
        };
        let failed = match joined {
            Joined::Done(Ok(Some(Ok(value)))) => {
                last = value;
                None
            }
            Joined::Done(Ok(Some(Err(err)))) => Some(err),
            // The listener's fiber unloaded mid-dispatch: a vanished listener
            // counts as absent, never as a failure the emitter caused.
            Joined::Done(Ok(None)) => None,
            Joined::Done(Err(join_err)) => Some(Error::listener_panicked(&join_err)),
            // Dropping the JoinHandle await detaches the task; the tracked
            // foreign call keeps running in the listener fiber's scope.
            Joined::Elapsed => Some(Error::listener_timeout(name)),
        };
        if let Some(err) = failed {
            crate::policy::react_to_listener_failure(ctx, entry.fiber.as_ref(), &err);
            failures.push(err);
        }
    }
    if failures.is_empty() {
        Ok(last)
    } else {
        Err(Error::aggregate(&failures))
    }
}

/// Sequential dispatch; stops at the first bail value.
///
/// # Errors
///
/// Propagates listener failures, and returns the bail value on bail.
async fn run_sequential(
    ctx: &Context,
    name: &str,
    mode: DispatchMode,
    payload: Value,
) -> Result<Value> {
    let budget = ctx.root.timeouts.listener;
    let dispatch = begin_dispatch(ctx, mode.as_str(), name);
    let mut current = payload;
    for entry in dispatch.entries {
        // A listener whose fiber unloaded between snapshot and invocation
        // counts as unregistered: skip it instead of running dead code.
        if entry.is_dead() {
            continue;
        }
        let ctx = ctx.clone();
        let value = if let Some(budget) = budget {
            match invoke_bounded(entry, name, ctx, current, null_next(), mode, budget).await {
                InvokeOutcome::Returned(result) => result?,
                // The listener's fiber vanished between the dead-check
                // and the tracked spawn: absent, never failed (S2); the
                // payload passes through unchanged.
                InvokeOutcome::Vanished(back) => {
                    current = back;
                    continue;
                }
            }
        } else {
            // Inline (unbudgeted) invocation: the S5 reaction wraps the
            // settled failure here; the budgeted arm reacts inside
            // `invoke_bounded` instead.
            let fiber = entry.fiber.clone();
            let react_ctx = ctx.clone();
            let result = entry.invoke(ctx, current, null_next(), mode).await;
            if let Err(err) = &result {
                crate::policy::react_to_listener_failure(&react_ctx, fiber.as_ref(), err);
            }
            result?
        };
        if is_bailed(&value) {
            return Ok(value);
        }
        current = value;
    }
    Ok(current)
}

pub async fn serial(ctx: &Context, name: &str, payload: Value) -> Result<Value> {
    run_sequential(ctx, name, DispatchMode::Serial, payload).await
}

/// First-bail-wins dispatch (async twin of cordis' synchronous `bail`).
///
/// # Errors
///
/// Propagates listener failures, and returns the bail value on bail.
pub async fn bail(ctx: &Context, name: &str, payload: Value) -> Result<Value> {
    run_sequential(ctx, name, DispatchMode::Bail, payload).await
}

/// Waterfall dispatch: listeners wrap a `next` continuation around the
/// fallback.
///
/// # Errors
///
/// Propagates listener or fallback failures.
pub async fn waterfall<F>(ctx: &Context, name: &str, payload: Value, fallback: F) -> Result<Value>
where
    F: Fn(Value) -> BoxFuture<Result<Value>> + Send + Sync + 'static,
{
    let budget = ctx.root.timeouts.listener;
    let name_owned = name.to_string();
    let dispatch = begin_dispatch(ctx, "waterfall", name);
    let fallback = Arc::new(fallback);
    let ctx_owned = ctx.clone();
    // Build the chain back-to-front: each listener wraps the continuation of
    // everything after it.
    let mut continuation: Arc<dyn Fn(Value) -> BoxFuture<Result<Value>> + Send + Sync> =
        Arc::new(move |payload| fallback(payload));
    for entry in dispatch.entries.into_iter().rev() {
        let inner = Arc::clone(&continuation);
        let hook_ctx = ctx_owned.clone();
        let name = name_owned.clone();
        continuation = Arc::new(move |payload: Value| {
            let inner = Arc::clone(&inner);
            let entry = entry.clone();
            let hook_ctx = hook_ctx.clone();
            let name = name.clone();
            Box::pin(async move {
                // A listener whose fiber unloaded mid-chain must not veto
                // the rest: skip straight to the remaining chain.
                if entry.is_dead() {
                    return inner(payload).await;
                }
                let next_inner = Arc::clone(&inner);
                let next = Next::new(Arc::new(move |payload| {
                    let inner = Arc::clone(&next_inner);
                    Box::pin(async move { inner(payload).await })
                }));
                if let Some(budget) = budget {
                    match invoke_bounded(
                        entry,
                        &name,
                        hook_ctx,
                        payload,
                        next,
                        DispatchMode::Waterfall,
                        budget,
                    )
                    .await
                    {
                        InvokeOutcome::Returned(result) => result,
                        // Vanished mid-chain: same as dead — no veto;
                        // the payload passes down the remaining chain.
                        InvokeOutcome::Vanished(back) => inner(back).await,
                    }
                } else {
                    // Inline (unbudgeted) invocation: same S5 reaction
                    // seam as `run_sequential`.
                    let fiber = entry.fiber.clone();
                    let react_ctx = hook_ctx.clone();
                    let result = entry
                        .invoke(hook_ctx, payload, next, DispatchMode::Waterfall)
                        .await;
                    if let Err(err) = &result {
                        crate::policy::react_to_listener_failure(&react_ctx, fiber.as_ref(), err);
                    }
                    result
                }
            })
        });
    }
    continuation(payload).await
}

/// Fire-and-forget dispatch restricted to internal events (no
/// `internal/dispatch` recursion). An optional scope filter decides which
/// hooks receive the event, mirroring TS filtered emission.
pub fn emit_internal(
    ctx: &Context,
    name: &str,
    payload: &Value,
    filter: Option<&dyn Fn(&Context) -> bool>,
) {
    let entries: Vec<DispatchEntry> = {
        let store = ctx.root.events.lock();
        store.matching(name, filter).entries
    };
    let budget = ctx.root.timeouts.listener;
    for entry in entries {
        let work = run_fire_and_forget(
            entry.clone(),
            name.to_string(),
            ctx.clone(),
            payload.clone(),
            budget,
        );
        // Detached fire-and-forget, bound to the listener fiber's scope (S2).
        let _ = entry.spawn_scoped(work);
    }
}

/// Synchronous waterfall used by internal hot paths (config/update): runs the
/// chain inline on the current task.
pub async fn waterfall_internal(
    ctx: &Context,
    name: &str,
    this: Option<Arc<Fiber>>,
    payload: Value,
    fallback: impl Fn(Value) -> BoxFuture<Result<Value>> + Send + Sync + 'static,
) -> Result<Value> {
    let listeners: Vec<Listener> = {
        let store = ctx.root.events.lock();
        store
            .hooks
            .get(name)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|hook| Arc::clone(&hook.callback))
            .collect()
    };
    let this_ctx = this
        .as_ref()
        .map_or_else(|| ctx.clone(), |fiber| fiber.ctx());
    let fallback = Arc::new(fallback);
    let mut continuation: Arc<dyn Fn(Value) -> BoxFuture<Result<Value>> + Send + Sync> =
        Arc::new(move |payload| fallback(payload));
    for listener in listeners.into_iter().rev() {
        let inner = Arc::clone(&continuation);
        let hook_ctx = this_ctx.clone();
        continuation = Arc::new(move |payload: Value| {
            let inner = Arc::clone(&inner);
            let listener = Arc::clone(&listener);
            let hook_ctx = hook_ctx.clone();
            Box::pin(async move {
                let next = Next::new(Arc::new(move |payload| {
                    let inner = Arc::clone(&inner);
                    Box::pin(async move { inner(payload).await })
                }));
                listener(hook_ctx, payload, next).await
            })
        });
    }
    continuation(payload).await
}

/// Shared registration path for plain listeners and mode-aware bridges.
async fn register_listener(
    ctx: &Context,
    name: &str,
    listener: Option<Listener>,
    bridge: Option<DispatchListener>,
    global: bool,
) -> Result<EffectGuard> {
    let op = if bridge.is_some() {
        "on_dispatch"
    } else {
        "on"
    };
    let fiber = ctx.require_fiber(op)?;
    let label = format!("ctx.{op}({name:?})");
    let name_owned = name.to_string();
    let register_ctx = ctx.clone();
    // For bridges this is an identity token only: removal matches on it, and
    // `DispatchEntry::invoke` never calls it while a bridge is present.
    let hook_callback: Listener =
        listener.unwrap_or_else(|| Arc::new(|_, _, _| Box::pin(async { Ok(Value::Null) })));
    let listener_for_dispose = Arc::clone(&hook_callback);
    let dispose_name = name_owned.clone();
    let dispose_ctx = register_ctx.clone();
    fiber
        .effect(
            label,
            // Listener removal is kernel-structural and synchronous (S3).
            crate::fiber::Teardown::Critical,
            async move {
                {
                    let mut store = register_ctx.root.events.lock();
                    store.register(
                        &name_owned,
                        register_ctx.clone(),
                        Arc::clone(&hook_callback),
                        global,
                        bridge,
                    );
                }
                let disposer: crate::fiber::Disposer = Box::new(move || {
                    let name = dispose_name.clone();
                    let listener = Arc::clone(&listener_for_dispose);
                    let ctx = dispose_ctx.clone();
                    Box::pin(async move {
                        ctx.root.events.lock().remove(&name, &listener);
                    })
                });
                Ok(Some(disposer))
            },
        )
        .await
}

/// Registers a listener owned by the current fiber.
///
/// # Errors
///
/// Fails when the current fiber cannot accept effects.
pub async fn on(
    ctx: &Context,
    name: &str,
    listener: Listener,
    global: bool,
) -> Result<EffectGuard> {
    register_listener(ctx, name, Some(listener), None, global).await
}

/// Registers a mode-aware bridge listener owned by the current fiber
/// (boundary adapters).
///
/// Bridges observe the dispatch mode of every invocation. Subscribing to
/// `internal/*` events is rejected: those are framework extension points
/// with plain-callback semantics.
///
/// # Errors
///
/// Fails on `internal/*` names or when the current fiber cannot accept
/// effects.
pub async fn on_bridge(
    ctx: &Context,
    name: &str,
    listener: DispatchListener,
) -> Result<EffectGuard> {
    if name.starts_with("internal/") {
        return Err(Error::msg(
            "dispatch bridges cannot subscribe to internal/* events",
        ));
    }
    register_listener(ctx, name, None, Some(listener), false).await
}

/// Registers a self-disposing one-shot listener.
///
/// # Errors
///
/// Fails when the current fiber cannot accept effects.
pub async fn once(ctx: &Context, name: &str, listener: Listener) -> Result<EffectGuard> {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let inner = Arc::clone(&listener);
    let once_listener: Listener = Arc::new(move |ctx: Context, payload: Value, next: Next| {
        let already = fired.swap(true, std::sync::atomic::Ordering::SeqCst);
        let inner = Arc::clone(&inner);
        Box::pin(async move {
            if already {
                return Ok(Value::Null);
            }
            inner(ctx, payload, next).await
        })
    });
    on(ctx, name, once_listener, false).await
}
