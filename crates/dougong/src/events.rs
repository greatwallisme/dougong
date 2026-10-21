// Rust guideline compliant 2026-10-20
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

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

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

/// Fire-and-forget dispatch. Listener failures are logged, never propagated.
pub fn emit(ctx: &Context, name: &str, payload: Value) {
    let ctx = ctx.clone();
    let name = name.to_string();
    tokio::spawn(async move {
        let dispatch = begin_dispatch(&ctx, "emit", &name);
        for entry in dispatch.entries {
            let ctx = ctx.clone();
            let payload = payload.clone();
            tokio::spawn(async move {
                let next = Next::new(Arc::new(|_| {
                    Box::pin(async { Ok(Value::Null) }) as BoxFuture<Result<Value>>
                }));
                if let Err(err) = entry
                    .invoke(ctx.clone(), payload, next, DispatchMode::Emit)
                    .await
                {
                    ctx.logger().log_error(&err);
                }
            });
        }
    });
}

/// Concurrent dispatch; waits for every listener and aggregates failures.
///
/// # Errors
///
/// Returns an aggregate error when any listener fails.
pub async fn parallel(ctx: &Context, name: &str, payload: Value) -> Result<Value> {
    let dispatch = begin_dispatch(ctx, "parallel", name);
    let mut tasks = Vec::with_capacity(dispatch.entries.len());
    for entry in dispatch.entries {
        let ctx = ctx.clone();
        let payload = payload.clone();
        tasks.push(tokio::spawn(async move {
            let next = Next::new(Arc::new(|_| {
                Box::pin(async { Ok(Value::Null) }) as BoxFuture<Result<Value>>
            }));
            entry
                .invoke(ctx, payload, next, DispatchMode::Parallel)
                .await
        }));
    }
    let mut failures = Vec::new();
    let mut last = payload;
    for task in tasks {
        match task.await {
            Ok(Ok(value)) => last = value,
            Ok(Err(err)) => failures.push(err),
            Err(join_err) => failures.push(Error::msg(format!("listener task failed: {join_err}"))),
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
    let dispatch = begin_dispatch(ctx, mode.as_str(), name);
    let mut current = payload;
    for entry in dispatch.entries {
        let ctx = ctx.clone();
        let next = Next::new(Arc::new(|_| {
            Box::pin(async { Ok(Value::Null) }) as BoxFuture<Result<Value>>
        }));
        let value = entry.invoke(ctx, current, next, mode).await?;
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
        continuation = Arc::new(move |payload: Value| {
            let inner = Arc::clone(&inner);
            let entry = entry.clone();
            let hook_ctx = hook_ctx.clone();
            Box::pin(async move {
                let next = Next::new(Arc::new(move |payload| {
                    let inner = Arc::clone(&inner);
                    Box::pin(async move { inner(payload).await })
                }));
                entry
                    .invoke(hook_ctx, payload, next, DispatchMode::Waterfall)
                    .await
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
    for entry in entries {
        let ctx = ctx.clone();
        let payload = payload.clone();
        tokio::spawn(async move {
            let next = Next::new(Arc::new(|_| {
                Box::pin(async { Ok(Value::Null) }) as BoxFuture<Result<Value>>
            }));
            if let Err(err) = entry
                .invoke(ctx.clone(), payload, next, DispatchMode::Emit)
                .await
            {
                ctx.logger().log_error(&err);
            }
        });
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
        .effect(label, async move {
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
        })
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
