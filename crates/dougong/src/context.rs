// Rust guideline compliant 2026-10-20
//! Context handles and shared root state.
//!
//! A [`Context`] is a cheap-clonable handle: every handle derived from one
//! root shares a [`RootState`] (registry, service store, event bus, fiber
//! table, dependency indexes), while each handle carries a *scope chain* for
//! isolation/interception and a weak link to its owning fiber. `extend`,
//! `isolate`, and `intercept` derive child contexts without mutating the
//! parent — the Rust translation of cordis' prototypal child contexts.

use parking_lot::Mutex;
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::events::{self, Listener};
use crate::fiber::{Disposer, EffectGuard, Fiber, FiberHandle, FiberState, Runtime};
use crate::logger::{Logger, LoggerHandle, StderrLogger};
use crate::plugin::{BoxFuture, Injection, Plugin};
use crate::registry;
use crate::scope::{DEFAULT_LABEL, ScopeNode};
use crate::service::{DependencyIndex, ReflectStore};
use crate::shard::ShardMap;

/// State shared by every context derived from one root.
///
/// The hot structures (`reflect`, `index`, `fibers`) are striped internally
/// (P1, `shard.rs`); the plugin registry and event store keep a single lock
/// (cold path), and the active set is the P4 lock-free bitmap mirror.
///
/// Lock-order rule: no root-state structure lock (stripe or single) is ever
/// held while acquiring another structure's lock or a fiber-internal lock —
/// cross-structure flows snapshot under one lock, release it, then act. An
/// S6 group-section lock is always the outermost lock.
pub struct RootState {
    pub(crate) registry: Mutex<registry::RegistryMap>,
    pub(crate) reflect: ReflectStore,
    pub(crate) events: Mutex<crate::events::EventStore>,
    /// Every live fiber by uid.
    pub(crate) fibers: ShardMap<u64, Weak<Fiber>>,
    /// The immortal root fiber (uid 0); owns built-in listeners/effects.
    pub(crate) root_fiber: std::sync::OnceLock<Arc<Fiber>>,
    pub(crate) counter: AtomicU64,
    pub(crate) label_seed: AtomicU64,
    /// Reverse indexes (design decision D12).
    pub(crate) index: DependencyIndex,
    /// Lock-free-friendly mirror of which fiber uids are `Active`.
    pub(crate) active: crate::shard::ActiveSet,
    /// Timeout budgets for the critical async paths (S3); fixed at
    /// construction so every reader sees one consistent policy set.
    pub(crate) timeouts: crate::timeout::TimeoutConfig,
    /// Plugin resource budgets (P7); likewise fixed at construction.
    pub(crate) budgets: crate::budget::BudgetConfig,
    /// Per-class error reactions (S5); likewise fixed at construction.
    pub(crate) policies: crate::policy::ErrorPolicies,
    /// Per-service-group serial sections for dependency updates (S6, D21).
    pub(crate) ordering: crate::ordering::UpdateSerializers,
    /// Bounded fire-and-forget listener scheduling (P2, D22).
    pub(crate) dispatcher: crate::event_queue::EventDispatcher,
    /// Runtime metrics (O2); counters always on, latency opt-in.
    pub(crate) metrics: crate::metrics::Metrics,
}

impl std::fmt::Debug for RootState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootState")
            .field("live_fibers", &self.fibers.total_entries())
            .finish_non_exhaustive()
    }
}

impl RootState {
    pub(crate) fn next_uid(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Allocates a fresh isolation label.
    #[must_use]
    pub fn fresh_label(&self) -> u64 {
        self.label_seed.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Strict-mode provider liveness: true when the fiber is the root or
    /// currently `Active`.
    #[must_use]
    pub fn fiber_is_active(&self, uid: u64) -> bool {
        if uid == 0 {
            return true;
        }
        self.active.contains(uid)
    }

    pub(crate) fn mark_active(&self, uid: u64) {
        self.active.mark(uid);
    }

    pub(crate) fn mark_inactive(&self, uid: u64) {
        self.active.unmark(uid);
    }

    pub(crate) fn register_fiber(&self, fiber: &Arc<Fiber>) {
        if let Some(uid) = fiber.uid_value() {
            self.fibers.write(&uid, |table| {
                table.insert(uid, Arc::downgrade(fiber));
            });
        }
    }

    /// Removes a disposed fiber: fiber table plus dependency indexes.
    pub(crate) fn remove_fiber(&self, uid: u64, inject_names: &[String]) {
        self.fibers.write(&uid, |table| {
            table.remove(&uid);
        });
        // The disposed fiber cannot refresh its own mirror any more (its uid
        // is cleared first), so drop the active-set entry here.
        self.active.unmark(uid);
        self.index.remove_fiber(uid, inject_names);
    }

    /// Upgrades the table entry for `uid` to a strong fiber reference.
    pub(crate) fn fiber_by_uid(&self, uid: u64) -> Option<Arc<Fiber>> {
        self.fibers.read(&uid, |slot| slot.and_then(Weak::upgrade))
    }

    pub(crate) fn context_of(&self, uid: u64) -> Option<Context> {
        self.fiber_by_uid(uid).map(|fiber| fiber.ctx())
    }
}

/// Dependency container handle; clone freely.
#[derive(Clone)]
pub struct Context {
    /// Shared root state.
    pub(crate) root: Arc<RootState>,
    /// Scope chain for isolation/interception.
    pub(crate) scope: Arc<ScopeNode>,
    /// Owning fiber (weak): the root context's fiber links to the root fiber.
    pub(crate) fiber: Weak<Fiber>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("live_fibers", &self.live_fibers())
            .finish_non_exhaustive()
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Construction-time options of a root context: timeout budgets (S3),
/// per-class error policies (S5), and event queue policies (P2).
///
/// Every field defaults to the pre-policy behavior (all budgets unbounded,
/// all errors logged at `Error`, no listener-failure reactions, no provider
/// quarantine, lossless unbounded reliable events), so a default-constructed
/// root behaves exactly as before.
#[derive(Debug, Clone, Default)]
pub struct ContextOptions {
    /// Timeout budgets for the critical async paths.
    pub timeouts: crate::timeout::TimeoutConfig,
    /// Plugin resource budgets (P7).
    pub budgets: crate::budget::BudgetConfig,
    /// Per-class error reactions.
    pub policies: crate::policy::ErrorPolicies,
    /// Event queue capacity, overflow, and concurrency policies.
    pub events: crate::event_queue::EventQueueConfig,
    /// Runtime metrics switches (O2); latency instrumentation is opt-in.
    pub metrics: crate::metrics::MetricsConfig,
}

impl Context {
    /// Creates the root context with every budget and policy disabled (the
    /// pre-S3/S5 behavior).
    ///
    /// The built-in `logger` service is seeded directly into the store (owned
    /// by the root fiber) so every plugin can log before providing anything.
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_options(ContextOptions::default())
    }

    /// Creates the root context with explicit timeout budgets (S3) and
    /// default error policies.
    ///
    /// The budgets are fixed for the lifetime of the root; see
    /// [`TimeoutConfig`](crate::TimeoutConfig) for the per-path semantics.
    #[must_use]
    pub fn new_with_timeouts(timeouts: crate::timeout::TimeoutConfig) -> Self {
        Self::new_with_options(ContextOptions {
            timeouts,
            budgets: crate::budget::BudgetConfig::default(),
            policies: crate::policy::ErrorPolicies::default(),
            events: crate::event_queue::EventQueueConfig::default(),
            metrics: crate::metrics::MetricsConfig::default(),
        })
    }

    /// Creates the root context with explicit options (S3 budgets + S5
    /// error policies), both fixed for the lifetime of the root.
    #[must_use]
    pub fn new_with_options(options: ContextOptions) -> Self {
        let root = Arc::new(RootState {
            registry: Mutex::new(HashMap::new()),
            reflect: ReflectStore::default(),
            events: Mutex::new(crate::events::EventStore::default()),
            fibers: ShardMap::new(),
            root_fiber: std::sync::OnceLock::new(),
            counter: AtomicU64::new(0),
            label_seed: AtomicU64::new(1),
            index: DependencyIndex::default(),
            active: crate::shard::ActiveSet::new(),
            timeouts: options.timeouts,
            budgets: options.budgets,
            policies: options.policies,
            ordering: crate::ordering::UpdateSerializers::default(),
            metrics: crate::metrics::Metrics::new(options.metrics.latencies),
            dispatcher: crate::event_queue::EventDispatcher::new(
                options.events,
                options.metrics.latencies,
            ),
        });
        let handle = Self {
            root: Arc::clone(&root),
            scope: ScopeNode::root(),
            fiber: Weak::new(),
        };

        let root_fiber = Fiber::new_root();
        root_fiber.bind(&handle);
        let _ = root.root_fiber.set(Arc::clone(&root_fiber));
        root.register_fiber(&root_fiber);

        // Seed the built-in logger (provider: root fiber, uid 0).
        let logger_value = Arc::new(LoggerHandle::new(Arc::new(StderrLogger)));
        root.reflect.seed(
            crate::service::Provided {
                name: "logger".to_string(),
                value: logger_value,
                provider_uid: 0,
                check: None,
                compute: None,
            },
            DEFAULT_LABEL,
        );

        Self {
            root,
            scope: handle.scope,
            fiber: Arc::downgrade(&root_fiber),
        }
    }

    // ------------------------------------------------------------------
    // Scoping
    // ------------------------------------------------------------------

    /// Derives a child handle bound to the same scope and fiber.
    #[must_use]
    pub fn extend(&self) -> Self {
        Self {
            root: Arc::clone(&self.root),
            scope: Arc::clone(&self.scope),
            fiber: Weak::clone(&self.fiber),
        }
    }

    /// Creates a child where `name` resolves against a fresh isolation label.
    #[must_use]
    pub fn isolate(&self, name: &str) -> Self {
        let label = self.root.fresh_label();
        self.isolate_with_label(name, label)
    }

    /// [`Context::isolate`] with an explicit label; the same label joins
    /// scopes.
    #[must_use]
    pub fn isolate_with_label(&self, name: &str, label: u64) -> Self {
        Self {
            root: Arc::clone(&self.root),
            scope: ScopeNode::isolated(&self.scope, name, label),
            fiber: Weak::clone(&self.fiber),
        }
    }

    /// Creates a child whose provides land in the realm `label` and whose
    /// injections prefer realm-local providers before falling back to the
    /// global default (D15). The same label joins one realm across many
    /// plugin starts.
    #[must_use]
    pub fn isolate_realm(&self, label: u64) -> Self {
        Self {
            root: Arc::clone(&self.root),
            scope: ScopeNode::realm_child(&self.scope, label),
            fiber: Weak::clone(&self.fiber),
        }
    }

    /// Allocates a fresh isolation label; pair with
    /// [`isolate_with_label`](Self::isolate_with_label) when several starts
    /// must join one realm by name (declarative hosts do exactly this).
    #[must_use]
    pub fn fresh_label(&self) -> u64 {
        self.root.fresh_label()
    }

    /// Adds an intercept-config layer for `name` on a child scope.
    #[must_use]
    pub fn intercept(&self, name: &str, config: Value) -> Self {
        Self {
            root: Arc::clone(&self.root),
            scope: ScopeNode::intercepted(&self.scope, name, config),
            fiber: Weak::clone(&self.fiber),
        }
    }

    /// Merged intercept config for `name` visible from this context.
    #[must_use]
    pub fn service_config(&self, name: &str) -> Value {
        crate::service::merged_intercept(&self.scope, name)
    }

    /// The effective isolation label for `name` at this scope.
    #[must_use]
    pub fn effective_label(&self, name: &str) -> u64 {
        ScopeNode::effective_label(Some(&self.scope), name)
    }

    /// The innermost realm label on this context's scope chain (D15), or
    /// `None` for the default label. Log records and status payloads use it
    /// as the scope field.
    #[must_use]
    pub(crate) fn realm_label(&self) -> Option<u64> {
        let mut node = Some(&self.scope);
        while let Some(scope) = node {
            if let Some(realm) = scope.realm {
                return Some(realm);
            }
            node = scope.parent.as_ref();
        }
        None
    }

    /// This context's trace id (O3): the owning fiber's uid as captured at
    /// construction, stable across dispose. The root context traces as `0`.
    /// Plugins and boundary hosts stamp it onto their own records to join
    /// them with the kernel's lifecycle chain for the same fiber.
    #[must_use]
    pub fn trace_id(&self) -> u64 {
        self.fiber.upgrade().map_or(0, |fiber| fiber.trace_id())
    }

    /// Takes a point-in-time metrics snapshot (O2).
    ///
    /// Latency summaries carry `count == 0` unless
    /// [`MetricsConfig::latencies`](crate::MetricsConfig::latencies) was
    /// enabled at construction. The fiber distribution walks the registry
    /// under its lock; it is a snapshot, not a consistent cut across
    /// concurrent transitions. Exporters (Prometheus/OpenTelemetry) are host
    /// wiring over this face.
    #[must_use]
    pub fn metrics(&self) -> crate::metrics::MetricsSnapshot {
        let root = &self.root;
        let mut snapshot = crate::metrics::MetricsSnapshot::default();
        {
            let registry = root.registry.lock();
            for runtime in registry.values() {
                let guards = runtime.fibers.lock();
                for fiber in guards.iter().filter_map(Weak::upgrade) {
                    match fiber.state() {
                        FiberState::Pending => snapshot.pending_fibers += 1,
                        FiberState::Loading => snapshot.loading_fibers += 1,
                        FiberState::Active => snapshot.active_fibers += 1,
                        FiberState::Failed => snapshot.failed_fibers += 1,
                        FiberState::Unloading => snapshot.unloading_fibers += 1,
                        FiberState::Disposed => {}
                    }
                }
            }
        }
        snapshot.live_fibers = snapshot.pending_fibers
            + snapshot.loading_fibers
            + snapshot.active_fibers
            + snapshot.failed_fibers
            + snapshot.unloading_fibers;
        snapshot.provider_count = root.reflect.provider_count();
        let (depth, dropped) = root.dispatcher.queue_stats();
        snapshot.queue_depth = depth as u64;
        snapshot.dropped_events = dropped;
        let metrics = &root.metrics;
        snapshot.state_transition_count = metrics.state_transitions.load(Ordering::Relaxed);
        snapshot.notify_count = metrics.notifies.load(Ordering::Relaxed);
        snapshot.dependency_rebuild_count = root.index.rebuild_count();
        snapshot.apply_retry_count = metrics.apply_retries.load(Ordering::Relaxed);
        snapshot.logic_error_count = metrics.logic_errors.load(Ordering::Relaxed);
        snapshot.timeout_error_count = metrics.timeout_errors.load(Ordering::Relaxed);
        snapshot.transport_error_count = metrics.transport_errors.load(Ordering::Relaxed);
        snapshot.resolution = metrics.resolution.summarize();
        snapshot.update = metrics.update.summarize();
        snapshot.cleanup = metrics.cleanup.summarize();
        snapshot.drain = metrics.drain.summarize();
        snapshot
    }

    // ------------------------------------------------------------------
    // Fiber access
    // ------------------------------------------------------------------

    pub(crate) fn fiber(&self) -> Option<Arc<Fiber>> {
        self.fiber.upgrade()
    }

    pub(crate) fn require_fiber(&self, op: &str) -> Result<Arc<Fiber>> {
        self.fiber().ok_or_else(|| {
            Error::msg(format!(
                "cannot {op} on this context: no active fiber is bound"
            ))
        })
    }

    // ------------------------------------------------------------------
    // Services
    // ------------------------------------------------------------------

    /// Typed service read: prefers this fiber's committed view, then the
    /// scoped store (provider must be active). Accessor entries are computed
    /// against this context on every read.
    ///
    /// When the S5 provider quarantine is enabled, an
    /// [`Invoker`](crate::Invoker) read wraps
    /// the handle so a transport-class call failure disposes the provider
    /// fiber; the wrapper is transparent to callers otherwise.
    #[must_use]
    pub fn get<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        let started = self.root.metrics.instant_if();
        let result = self.resolve_typed(name);
        if let Some(started) = started {
            self.root
                .metrics
                .record(|metrics| &metrics.resolution, started);
        }
        result
    }

    /// The `get` body, split out so the O2 latency wrapper measures exactly
    /// one resolution including every early return.
    fn resolve_typed<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        if let Some(fiber) = self.fiber() {
            let snapshot = fiber.store.lock().get(name).cloned();
            if let Some(snapshot) = snapshot {
                if snapshot.dynamic {
                    // Fall through to the live computation below.
                } else {
                    let value = crate::policy::maybe_quarantine(
                        &self.root,
                        snapshot.provider_uid,
                        name,
                        snapshot.value,
                    );
                    return value.downcast::<T>().ok();
                }
            } else if fiber.declares(name) {
                // Declared but not committed: inactive dependency.
                return None;
            }
        }
        let label = self.effective_label(name);
        let provided = self.root.reflect.get_impl(&self.root, name, label, true)?;
        if let Some(compute) = &provided.compute {
            let computed = compute(self)?;
            return crate::policy::maybe_quarantine(
                &self.root,
                provided.provider_uid,
                name,
                computed,
            )
            .downcast::<T>()
            .ok();
        }
        crate::policy::maybe_quarantine(&self.root, provided.provider_uid, name, provided.value)
            .downcast::<T>()
            .ok()
    }

    /// Typed read or a descriptive error (TS proxy-miss diagnostics).
    ///
    /// # Errors
    ///
    /// Returns `inactive_dependency` for declared-but-inactive services and
    /// `undeclared_dependency` otherwise.
    pub fn require<T: Any + Send + Sync>(&self, name: &str) -> Result<Arc<T>> {
        let declared = self.fiber().is_some_and(|fiber| fiber.declares(name));
        let value = self.get::<T>(name);
        match (value, declared) {
            (Some(value), _) => Ok(value),
            (None, true) => Err(Error::inactive_dependency(name)),
            (None, false) => Err(Error::undeclared_dependency(name)),
        }
    }

    /// Registers a provided service owned by the current fiber.
    ///
    /// # Errors
    ///
    /// Fails when no fiber is bound, the fiber cannot accept effects, or the
    /// `(name, realm)` slot is occupied.
    pub async fn provide<T>(&self, name: &str, value: T) -> Result<EffectGuard>
    where
        T: Any + Send + Sync,
    {
        crate::service::provide(self, name, Arc::new(value), None).await
    }

    /// [`Context::provide`] with an availability predicate.
    ///
    /// # Errors
    ///
    /// Same conditions as [`Context::provide`].
    pub async fn provide_with_check<T>(
        &self,
        name: &str,
        value: T,
        check: Arc<dyn Fn(&Self) -> bool + Send + Sync>,
    ) -> Result<EffectGuard>
    where
        T: Any + Send + Sync,
    {
        crate::service::provide(self, name, Arc::new(value), Some(check)).await
    }

    /// Registers a computed service owned by the current fiber; every read
    /// re-evaluates `compute` against the reading context.
    ///
    /// # Errors
    ///
    /// Same conditions as [`Context::provide`].
    pub async fn accessor<T, F>(&self, name: &str, compute: F) -> Result<EffectGuard>
    where
        T: Any + Send + Sync,
        F: Fn(&Self) -> Option<Arc<T>> + Send + Sync + 'static,
    {
        let wrapped = Arc::new(move |ctx: &Self| {
            compute(ctx).map(|value| Arc::new(value) as Arc<dyn Any + Send + Sync>)
        });
        crate::service::accessor(self, name, wrapped).await
    }

    /// Overwrites the value of a service this fiber provided.
    ///
    /// # Errors
    ///
    /// Fails when the service is not provided by this fiber.
    pub fn set<T>(&self, name: &str, value: T) -> Result<()>
    where
        T: Any + Send + Sync,
    {
        crate::service::set(self, name, Arc::new(value))
    }

    /// Resolves the `logger` service, falling back to the built-in
    /// standard-error logger when absent.
    #[must_use]
    pub fn logger(&self) -> Arc<dyn Logger> {
        match self.get::<LoggerHandle>("logger") {
            Some(handle) => Arc::clone(&handle.0),
            None => Arc::new(StderrLogger),
        }
    }

    /// Raw typed value downcast for dynamic callers holding an erased value.
    #[must_use]
    pub fn downcast<T: Any + Send + Sync>(value: &Arc<dyn Any + Send + Sync>) -> Option<Arc<T>> {
        Arc::clone(value).downcast::<T>().ok()
    }

    // ------------------------------------------------------------------
    // Effects
    // ------------------------------------------------------------------

    /// Registers a cleanup-aware effect on the current fiber.
    ///
    /// # Errors
    ///
    /// Fails when the fiber cannot accept effects or `setup` fails.
    pub async fn effect<F>(&self, label: impl Into<String>, setup: F) -> Result<EffectGuard>
    where
        F: std::future::Future<Output = Result<Option<Disposer>>> + Send,
    {
        let fiber = self.require_fiber("effect")?;
        // Plugin-registered teardown is orphanable under the S3 cleanup
        // budget; kernel-structural effects register Critical directly.
        fiber
            .effect(label, crate::fiber::Teardown::Orphanable, setup)
            .await
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Starts a plugin in the current context; returns its fiber handle.
    ///
    /// The child fiber registers as an effect on the parent fiber: unloading
    /// the parent cascades to the child (paper Eq. 31).
    ///
    /// # Errors
    ///
    /// Fails when the parent fiber is disposed or unloading.
    pub fn plugin(&self, plugin: Arc<dyn Plugin>, config: Option<Value>) -> Result<FiberHandle> {
        registry::plugin(self, plugin, config)
    }

    /// Starts an anonymous callback once the named services are available.
    ///
    /// # Errors
    ///
    /// Fails when the parent fiber cannot accept the cascade effect.
    pub fn inject<F>(&self, deps: Vec<Injection>, callback: F) -> Result<FiberHandle>
    where
        F: Fn(Self) -> BoxFuture<Result<()>> + Send + Sync + 'static,
    {
        registry::inject(self, deps, callback)
    }

    /// Looks up the fiber handle of a plugin, if it has live fibers.
    #[must_use]
    pub fn fiber_handle(&self, plugin: &Arc<dyn Plugin>) -> Option<FiberHandle> {
        let runtime = registry::get(&self.root.registry, plugin)?;
        let fiber = runtime.fibers.lock().iter().find_map(Weak::upgrade)?;
        Some(FiberHandle(fiber))
    }

    /// Disposes every live fiber of `plugin` and removes its runtime record.
    pub async fn uninstall(&self, plugin: &Arc<dyn Plugin>) {
        registry::delete(self, plugin).await;
    }

    /// Disposes every running plugin fiber, newest registration first.
    pub async fn stop(&self) {
        let fibers: Vec<Arc<Fiber>> = {
            let registry = self.root.registry.lock();
            let mut fibers = Vec::new();
            for runtime in registry.values() {
                let guards = runtime.fibers.lock();
                for fiber in guards.iter().rev().filter_map(Weak::upgrade) {
                    fibers.push(fiber);
                }
            }
            drop(registry);
            fibers
        };
        for fiber in fibers {
            fiber.dispose().await;
        }
    }

    // ------------------------------------------------------------------
    // Events (mixed onto the context like cordis mixins)
    // ------------------------------------------------------------------

    /// Registers a listener owned by the current fiber.
    ///
    /// # Errors
    ///
    /// Fails when the current fiber cannot accept effects.
    pub async fn on(&self, name: &str, listener: Listener) -> Result<EffectGuard> {
        events::on(self, name, listener, false).await
    }

    /// Registers a global listener (ignores scope filters).
    ///
    /// # Errors
    ///
    /// Fails when the current fiber cannot accept effects.
    pub async fn on_global(&self, name: &str, listener: Listener) -> Result<EffectGuard> {
        events::on(self, name, listener, true).await
    }

    /// Registers a mode-aware bridge listener (boundary adapters). Bridges
    /// observe the dispatch mode of every invocation; `internal/*` names are
    /// rejected.
    ///
    /// # Errors
    ///
    /// Fails on `internal/*` names or when the current fiber cannot accept
    /// effects.
    pub async fn on_dispatch(
        &self,
        name: &str,
        listener: crate::events::DispatchListener,
    ) -> Result<EffectGuard> {
        events::on_bridge(self, name, listener).await
    }

    /// Registers a self-disposing one-shot listener.
    ///
    /// # Errors
    ///
    /// Fails when the current fiber cannot accept effects.
    pub async fn once(&self, name: &str, listener: Listener) -> Result<EffectGuard> {
        events::once(self, name, listener).await
    }

    /// Concurrent dispatch; aggregates listener failures.
    ///
    /// # Errors
    ///
    /// Returns an aggregate error when any listener fails.
    pub async fn parallel(&self, name: &str, payload: Value) -> Result<Value> {
        events::parallel(self, name, payload).await
    }

    /// Fire-and-forget dispatch; failures are logged.
    pub fn emit(&self, name: &str, payload: Value) {
        events::emit(self, name, payload);
    }

    /// Enqueues an event with async backpressure or explicit drop policy.
    ///
    /// Reliable synchronous [`emit`](Self::emit) remains non-blocking and
    /// lossless. Use this async API when a bounded backpressure policy must
    /// wait for queue capacity; drop policies report dropped listener work.
    #[must_use]
    pub async fn emit_with_policy(
        &self,
        name: &str,
        payload: Value,
    ) -> crate::event_queue::EmitOutcome {
        events::emit_with_policy(self, name, payload).await
    }

    /// Sequential dispatch until the first bail value.
    ///
    /// # Errors
    ///
    /// Propagates listener failures; returns the bail value on bail.
    pub async fn serial(&self, name: &str, payload: Value) -> Result<Value> {
        events::serial(self, name, payload).await
    }

    /// First-bail-wins dispatch (async twin of cordis' synchronous `bail`).
    ///
    /// # Errors
    ///
    /// Propagates listener failures; returns the bail value on bail.
    pub async fn bail(&self, name: &str, payload: Value) -> Result<Value> {
        events::bail(self, name, payload).await
    }

    /// Waterfall dispatch around a fallback continuation.
    ///
    /// # Errors
    ///
    /// Propagates listener or fallback failures.
    pub async fn waterfall<F>(&self, name: &str, payload: Value, fallback: F) -> Result<Value>
    where
        F: Fn(Value) -> BoxFuture<Result<Value>> + Send + Sync + 'static,
    {
        events::waterfall(self, name, payload, fallback).await
    }

    // ------------------------------------------------------------------
    // Introspection
    // ------------------------------------------------------------------

    /// The timeout budgets this root was built with (S3).
    #[must_use]
    pub fn timeouts(&self) -> &crate::timeout::TimeoutConfig {
        &self.root.timeouts
    }

    /// The per-class error policies this root was built with (S5).
    #[must_use]
    pub fn policies(&self) -> &crate::policy::ErrorPolicies {
        &self.root.policies
    }

    /// Number of registered plugin runtimes.
    #[must_use]
    pub fn registry_size(&self) -> usize {
        registry::size(&self.root.registry)
    }

    /// Number of live fibers known to this root.
    #[must_use]
    pub fn live_fibers(&self) -> usize {
        let mut live = 0;
        self.root.fibers.for_each_shard(|table| {
            live += table
                .values()
                .filter(|weak| weak.upgrade().is_some())
                .count();
        });
        live
    }

    /// True when `uid` is an active fiber (root is always active).
    #[must_use]
    pub fn fiber_is_active(&self, uid: u64) -> bool {
        self.root.fiber_is_active(uid)
    }

    /// True when at least one committed consumer still references the fiber
    /// `uid` as a provider (the drain-protocol predicate, paper Eq. 46).
    #[must_use]
    pub fn relied(&self, uid: u64) -> bool {
        self.root.index.relied(uid)
    }

    /// True when `plugin` has at least one live fiber.
    #[must_use]
    pub fn has_plugin(&self, plugin: &Arc<dyn Plugin>) -> bool {
        registry::has(&self.root.registry, plugin)
    }

    /// The current lifecycle state of the owning fiber, when bound.
    #[must_use]
    pub fn fiber_state(&self) -> Option<FiberState> {
        self.fiber().map(|fiber| fiber.state())
    }

    /// The uid of the owning fiber, when bound; boundary adapters report it
    /// as the activation-site identity.
    #[must_use]
    pub fn fiber_uid(&self) -> Option<u64> {
        self.fiber().and_then(|fiber| fiber.uid_value())
    }

    /// The committed epoch of the owning fiber ([`EPOCH_INACTIVE`] while not
    /// loaded).
    ///
    /// [`EPOCH_INACTIVE`]: crate::EPOCH_INACTIVE
    #[must_use]
    pub fn fiber_epoch(&self) -> Option<String> {
        self.fiber().map(|fiber| fiber.epoch())
    }

    /// The resolved runtime for a plugin, if registered.
    #[must_use]
    pub fn plugin_runtime(&self, plugin: &Arc<dyn Plugin>) -> Option<Arc<Runtime>> {
        registry::get(&self.root.registry, plugin)
    }
}
