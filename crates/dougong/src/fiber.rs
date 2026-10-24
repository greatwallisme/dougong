// Rust guideline compliant 2026-10-20
//! Fibers: live plugin instances with epoch-driven convergence.
//!
//! Semantics (paper §4, TS `fiber.ts`, decisions D4/D12):
//!
//! - states: `Pending -> Loading -> Active` and `Active -> Unloading ->
//!   Pending` (plus `Failed` parked until dependency changes re-arm, and the
//!   terminal `Disposed` state).
//! - the driver task serializes transitions per fiber; a target change during
//!   an in-flight transition is handled after landing (inertial convergence):
//!   the driver loops until `loaded == desired`.
//! - unloading runs collected disposers LIFO; loading rolls back collected
//!   effects when the body errors (transactional activation).
//! - child fibers register as effects on the parent fiber, so unloading a
//!   parent cascades (paper Eq. 31; TS `dispose = parent.fiber.effect(...)`).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use serde_json::Value;

use crate::cancel::CancelScope;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::events;
use crate::invariant;
use crate::logger::LogLevel;
use crate::plugin::{BoxFuture, Injection, Plugin};
use crate::scope::ScopeNode;
use crate::service::resolve_impl_for;

/// A boxed async cleanup routine collected as a fiber effect.
pub type Disposer = Box<dyn FnOnce() -> BoxFuture<()> + Send>;

/// Whether a disposer may outlive its fiber's unload path (S3 cleanup
/// budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    /// Kernel-structural disposers (service withdrawal, listener removal,
    /// parent cascade): never orphaned — the unload path always awaits them;
    /// their inner waits are bounded by their own budgets (drain) or are
    /// synchronous.
    Critical,
    /// Plugin-registered teardown ([`Context::effect`], boundary adapters):
    /// when the cleanup budget fires the disposer keeps running on a detached
    /// task and the LIFO drain continues (see [`crate::TimeoutConfig::cleanup`]).
    Orphanable,
}

/// One collected effect slot: id, label, teardown criticality, disposer.
type Disposable = Option<(u64, String, Teardown, Disposer)>;

/// Sentinel epoch string meaning "required services are not all available".
pub const EPOCH_INACTIVE: &str = "";

/// The terminal state of one activation attempt (S3).
enum LoadOutcome {
    /// The fiber landed (loaded, or failed and parked) — keep converging.
    Landed,
    /// The body hit the apply budget and was rolled back; the driver decides
    /// between retry and parking `Failed`.
    TimedOut,
}

/// How one spawned body task settled (S3).
enum BodyOutcome {
    /// The body returned (activation result).
    Completed(Result<()>),
    /// The scope was cancelled before or while the body ran.
    Cancelled,
    /// The body task died (panic or external abort).
    Panicked(tokio::task::JoinError),
    /// The apply budget fired; the task was cancelled and awaited to
    /// settlement, and any late result was discarded.
    TimedOut,
}

/// Lifecycle state of one fiber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FiberState {
    /// Registered; waiting for required services.
    Pending = 0,
    /// The plugin body is running.
    Loading = 1,
    /// The body completed and all effects are live.
    Active = 2,
    /// Startup failed; parked until dependency changes re-arm convergence.
    Failed = 3,
    /// Draining effects LIFO after deactivation.
    Unloading = 4,
    /// Fully disposed; uid cleared.
    Disposed = 5,
}

/// Runtime record shared by all fibers of one plugin entrypoint.
pub struct Runtime {
    /// Registry identity (derived from the plugin handle identity).
    pub(crate) key: u64,
    /// Display name (from the first registration).
    pub name: Option<String>,
    /// The plugin entrypoint.
    pub(crate) plugin: Arc<dyn Plugin>,
    /// Live fibers of this plugin.
    pub(crate) fibers: Mutex<Vec<Weak<Fiber>>>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("key", &self.key)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Snapshot of one committed dependency: provider identity plus value.
#[derive(Clone)]
pub struct ProviderSnapshot {
    /// Fiber uid of the provider at commit time.
    pub provider_uid: u64,
    /// The committed value; accessor entries recompute per read.
    pub value: Arc<dyn std::any::Any + Send + Sync>,
    /// Accessor entries recompute per read.
    pub dynamic: bool,
}

impl ProviderSnapshot {
    pub(crate) fn dynamic(provider_uid: u64) -> Self {
        Self {
            provider_uid,
            value: Arc::new(()),
            dynamic: true,
        }
    }
}

impl std::fmt::Debug for ProviderSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The erased value is not Debug; identity is what matters here.
        f.debug_struct("ProviderSnapshot")
            .field("provider_uid", &self.provider_uid)
            .field("dynamic", &self.dynamic)
            .finish_non_exhaustive()
    }
}

pub struct DriveState {
    desired: String,
    dirty: bool,
    force: bool,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Apply-timeout retries already spent on the current desired epoch (S3);
    /// reset whenever the desired epoch changes or a load succeeds.
    retries_used: u32,
}

/// Live instance of one plugin application.
pub struct Fiber {
    /// Unique id (0 = root); `None` once disposed.
    pub uid: Mutex<Option<u64>>,
    pub(crate) runtime: Option<Arc<Runtime>>,
    pub(crate) inject: Vec<Injection>,
    pub(crate) scope: Arc<ScopeNode>,
    parent: Mutex<Weak<Self>>,
    ctx_slot: OnceLock<Context>,
    raw_config: Mutex<Value>,
    config: Mutex<Value>,
    error: Mutex<Option<Error>>,
    /// Candidate dependency snapshots (TS `_store`); committed at activation.
    pub(crate) candidates: Mutex<HashMap<String, ProviderSnapshot>>,
    /// Committed view ω: injected deps as of activation + self-provided.
    pub(crate) store: Mutex<HashMap<String, ProviderSnapshot>>,
    loaded: Mutex<String>,
    disposables: Mutex<Vec<Disposable>>,
    next_effect_id: AtomicU64,
    driver: Mutex<DriveState>,
    /// Current cancel-scope generation (S2): every task this fiber owns —
    /// listener invocations and the plugin body — is spawned into it, and
    /// unloading the activation cancels and reaps it (see `cancel.rs`).
    task_scope: Mutex<Arc<CancelScope>>,
    settle: (
        tokio::sync::watch::Sender<u64>,
        tokio::sync::watch::Receiver<u64>,
    ),
    state_cache: AtomicU8,
}

impl std::fmt::Debug for Fiber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fiber")
            .field("uid", &self.uid)
            .field("state", &self.state())
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

impl Fiber {
    pub(crate) fn new_root() -> Arc<Self> {
        Arc::new(Self {
            uid: Mutex::new(Some(0)),
            runtime: None,
            inject: Vec::new(),
            scope: ScopeNode::root(),
            parent: Mutex::new(Weak::new()),
            ctx_slot: OnceLock::new(),
            raw_config: Mutex::new(Value::Null),
            config: Mutex::new(Value::Null),
            error: Mutex::new(None),
            candidates: Mutex::new(HashMap::new()),
            store: Mutex::new(HashMap::new()),
            loaded: Mutex::new(EPOCH_INACTIVE.to_string()),
            disposables: Mutex::new(Vec::new()),
            next_effect_id: AtomicU64::new(1),
            driver: Mutex::new(DriveState {
                desired: EPOCH_INACTIVE.to_string(),
                dirty: false,
                force: false,
                task: None,
                retries_used: 0,
            }),
            task_scope: Mutex::new(Arc::new(CancelScope::new())),
            settle: tokio::sync::watch::channel(0),
            state_cache: AtomicU8::new(FiberState::Active as u8),
        })
    }

    pub(crate) fn new_child(
        runtime: Arc<Runtime>,
        inject: Vec<Injection>,
        config: Value,
        uid: u64,
        parent: &Arc<Self>,
        scope: Arc<ScopeNode>,
    ) -> Arc<Self> {
        Arc::new(Self {
            uid: Mutex::new(Some(uid)),
            runtime: Some(runtime),
            inject,
            scope,
            parent: Mutex::new(Arc::downgrade(parent)),
            ctx_slot: OnceLock::new(),
            raw_config: Mutex::new(config),
            config: Mutex::new(Value::Null),
            error: Mutex::new(None),
            candidates: Mutex::new(HashMap::new()),
            store: Mutex::new(HashMap::new()),
            loaded: Mutex::new(EPOCH_INACTIVE.to_string()),
            disposables: Mutex::new(Vec::new()),
            next_effect_id: AtomicU64::new(1),
            driver: Mutex::new(DriveState {
                desired: EPOCH_INACTIVE.to_string(),
                dirty: false,
                force: false,
                task: None,
                retries_used: 0,
            }),
            task_scope: Mutex::new(Arc::new(CancelScope::new())),
            settle: tokio::sync::watch::channel(0),
            state_cache: AtomicU8::new(FiberState::Pending as u8),
        })
    }

    /// Binds the owning context exactly once, right after construction.
    pub(crate) fn bind(self: &Arc<Self>, ctx: &Context) {
        let _ = self.ctx_slot.set(ctx.clone());
    }

    /// The bound context of this fiber.
    ///
    /// # Panics
    ///
    /// Panics if the context was never bound. Kernel constructors bind every
    /// fiber before publishing it, so this only trips on hand-built fibers.
    #[must_use]
    pub fn ctx(&self) -> Context {
        self.ctx_slot
            .get()
            .cloned()
            .expect("fiber context is bound at creation")
    }

    pub(crate) const fn scope_node(&self) -> &Arc<ScopeNode> {
        &self.scope
    }

    /// The plugin display name; nearest named runtime (walking parents), else
    /// `"root"`.
    #[must_use]
    pub fn name(&self) -> String {
        if let Some(runtime) = &self.runtime
            && let Some(name) = &runtime.name
        {
            return name.clone();
        }
        let mut cursor = self.parent.lock().upgrade();
        while let Some(fiber) = cursor {
            if let Some(runtime) = &fiber.runtime
                && let Some(name) = &runtime.name
            {
                return name.clone();
            }
            cursor = fiber.parent.lock().upgrade();
        }
        "root".to_string()
    }

    /// The uid, or `None` when disposed.
    pub(crate) fn uid_value(&self) -> Option<u64> {
        *self.uid.lock()
    }

    /// The driver's `(desired epoch, dirty flag)` snapshot.
    pub(crate) fn driver_snapshot(&self) -> (String, bool) {
        let driver = self.driver.lock();
        (driver.desired.clone(), driver.dirty)
    }

    /// True while a startup error is recorded.
    pub(crate) fn has_error(&self) -> bool {
        self.error.lock().is_some()
    }

    /// Compares the loaded epoch against `epoch` without cloning it.
    pub(crate) fn loaded_eq(&self, epoch: &str) -> bool {
        *self.loaded.lock() == epoch
    }

    /// The fiber's current cancel-scope generation (S2).
    pub(crate) fn current_scope(&self) -> Arc<CancelScope> {
        Arc::clone(&self.task_scope.lock())
    }

    /// Swaps in a fresh scope generation; called once per activation attempt.
    fn reset_scope(&self) -> Arc<CancelScope> {
        let fresh = Arc::new(CancelScope::new());
        *self.task_scope.lock() = Arc::clone(&fresh);
        fresh
    }

    /// Scope-draining predicate for the invariant layer: a cancelled scope
    /// must have reached zero live tasks by the time a transition lands.
    pub(crate) fn scope_reaped(&self) -> bool {
        let scope = self.current_scope();
        !scope.is_cancelled() || scope.task_count() == 0
    }

    /// Test-only driver override for invariant-layer fault injection.
    #[cfg(test)]
    pub(crate) fn test_set_driver(&self, desired: String, dirty: bool) {
        let mut driver = self.driver.lock();
        driver.desired = desired;
        driver.dirty = dirty;
    }

    /// The committed epoch string; [`EPOCH_INACTIVE`] while the fiber is not
    /// loaded.
    ///
    /// [`EPOCH_INACTIVE`]: crate::EPOCH_INACTIVE
    #[must_use]
    pub fn epoch(&self) -> String {
        self.loaded.lock().clone()
    }

    /// Names of the declared service dependencies.
    #[must_use]
    pub fn inject_names(&self) -> Vec<String> {
        self.inject
            .iter()
            .map(|injection| injection.service.to_string())
            .collect()
    }

    /// True when `name` is a declared dependency of this fiber.
    #[must_use]
    pub fn declares(&self, name: &str) -> bool {
        self.inject
            .iter()
            .any(|injection| injection.service.as_ref() == name)
    }

    /// Throws [`Error::inactive_effect`] when the fiber is disposed.
    pub(crate) fn assert_active(&self) -> Result<()> {
        if self.uid_value().is_some() {
            Ok(())
        } else {
            Err(Error::inactive_effect())
        }
    }

    fn bump_settle(&self) {
        let (tx, _) = &self.settle;
        tx.send_modify(|value| *value += 1);
    }

    /// Current lifecycle state: a lock-free read of the atomic mirror.
    #[must_use]
    pub fn state(&self) -> FiberState {
        match self.state_cache.load(Ordering::Acquire) {
            0 => FiberState::Pending,
            1 => FiberState::Loading,
            2 => FiberState::Active,
            3 => FiberState::Failed,
            4 => FiberState::Unloading,
            _ => FiberState::Disposed,
        }
    }

    /// Derives the converged state from live flags (TS `_getState`).
    fn compute_state(&self) -> FiberState {
        if self.uid_value().is_none() {
            return FiberState::Disposed;
        }
        let has_error = self.error.lock().is_some();
        let loaded = self.loaded.lock().clone();
        let driver = self.driver.lock();
        let busy = driver.dirty || loaded != driver.desired;
        let desired_inactive = driver.desired == EPOCH_INACTIVE;
        drop(driver);
        if has_error && loaded == EPOCH_INACTIVE && !busy {
            return FiberState::Failed;
        }
        if busy {
            return if desired_inactive {
                FiberState::Unloading
            } else {
                FiberState::Loading
            };
        }
        if loaded != EPOCH_INACTIVE {
            return FiberState::Active;
        }
        FiberState::Pending
    }

    /// Republishes the derived state into the atomic mirror; also maintains
    /// the root active set consulted by strict service resolution.
    fn cache_state(&self) {
        let state = self.compute_state();
        self.state_cache.store(state as u8, Ordering::Release);
        let root = &self.ctx().root;
        if let Some(uid) = self.uid_value() {
            if state == FiberState::Active {
                root.mark_active(uid);
            } else {
                root.mark_inactive(uid);
            }
        }
    }

    /// Emits `internal/status` with the current snapshot.
    fn publish_state(&self) {
        let payload = serde_json::json!({
            "fiber": self.name(),
            "uid": self.uid_value(),
            "state": self.state().as_str(),
        });
        events::emit_internal(&self.ctx(), "internal/status", &payload, None);
    }
}

impl FiberState {
    /// The stable lowercase name used in events and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Loading => "loading",
            Self::Active => "active",
            Self::Failed => "failed",
            Self::Unloading => "unloading",
            Self::Disposed => "disposed",
        }
    }
}

// ---------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------

impl Fiber {
    /// Registers a cleanup-aware effect on this fiber.
    ///
    /// `setup` runs immediately; its disposer is collected and runs (LIFO)
    /// when the guard is disposed eagerly or when the fiber unloads, whichever
    /// comes first. Disposal is single-shot and idempotent.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::inactive_effect`] when the fiber is disposed or
    /// unloading, or when `setup` itself fails.
    fn ensure_can_effect(&self) -> Result<()> {
        self.assert_active()?;
        if matches!(self.state(), FiberState::Unloading | FiberState::Disposed) {
            return Err(Error::inactive_effect());
        }
        Ok(())
    }

    /// Synchronously registers an already-built disposer as an effect.
    pub(crate) fn register_effect(
        self: &Arc<Self>,
        label: impl Into<String>,
        teardown: Teardown,
        disposer: Option<Disposer>,
    ) -> Result<EffectGuard> {
        self.ensure_can_effect()?;
        let effect_label = label.into();
        let id = self.next_effect_id.fetch_add(1, Ordering::SeqCst);
        if let Some(disposer) = disposer {
            let mut list = self.disposables.lock();
            list.push(Some((id, effect_label, teardown, disposer)));
        }
        Ok(EffectGuard {
            fiber: Arc::downgrade(self),
            id,
        })
    }

    /// Registers a cleanup-aware effect: `setup` runs immediately; its
    /// disposer is collected (see [`Fiber::register_effect`]).
    pub(crate) async fn effect(
        self: &Arc<Self>,
        label: impl Into<String>,
        teardown: Teardown,
        setup: impl std::future::Future<Output = Result<Option<Disposer>>> + Send,
    ) -> Result<EffectGuard> {
        self.ensure_can_effect()?;
        let effect_label = label.into();
        let disposer = setup.await?;
        self.register_effect(effect_label, teardown, disposer)
    }

    /// Runs one pending disposer by id, if still live.
    ///
    /// Disposer failures are logged and swallowed: cleanup errors are
    /// observable, never actionable (matching TS logger-swallow semantics).
    async fn run_disposer(self: &Arc<Self>, id: u64) {
        let taken = {
            let mut list = self.disposables.lock();
            let slot = list
                .iter_mut()
                .find(|slot| matches!(slot, Some((entry_id, _, _, _)) if *entry_id == id));
            let taken = slot
                .and_then(std::option::Option::take)
                .map(|(_, label, teardown, disposer)| (label, teardown, disposer));
            drop(list);
            taken
        };
        if let Some((label, teardown, disposer)) = taken {
            self.run_one_disposer(label, teardown, disposer).await;
            self.bump_settle();
        }
    }

    /// Runs one disposer under the cleanup budget (S3).
    ///
    /// Critical disposers always run inline to completion. An orphanable
    /// disposer under a configured budget runs on a detached task: on timeout
    /// the task is *not* aborted (aborting a wasm teardown would drop an
    /// in-flight call, and a synchronous foreign call cannot be preempted) —
    /// the timeout is logged and the drain continues while the disposer
    /// finishes in the background.
    async fn run_one_disposer(&self, label: String, teardown: Teardown, disposer: Disposer) {
        let budget = self.ctx().root.timeouts.cleanup;
        let Some(budget) = budget.filter(|_| teardown == Teardown::Orphanable) else {
            disposer().await;
            return;
        };
        let fiber = self.name();
        let uid = self.uid_value();
        let logger = self.ctx().logger();
        let task = tokio::spawn(disposer());
        match tokio::time::timeout(budget, task).await {
            Ok(Ok(())) => {}
            Ok(Err(join_error)) => {
                logger.log(
                    LogLevel::Error,
                    "dougong",
                    &format!(
                        "cleanup error: disposer \"{label}\" of fiber \"{fiber}\" (uid {uid:?}) failed: {join_error}"
                    ),
                );
            }
            Err(_elapsed) => {
                logger.log(
                    LogLevel::Error,
                    "dougong",
                    &format!(
                        "cleanup_timeout: disposer \"{label}\" of fiber \"{fiber}\" (uid {uid:?}) exceeded {budget:?}; orphaned (still running in the background)"
                    ),
                );
            }
        }
    }

    /// Drains every live effect LIFO, sequentially.
    async fn unload_effects(self: &Arc<Self>) {
        let disposers: Vec<(String, Teardown, Disposer)> = {
            let mut list = self.disposables.lock();
            let mut disposers = Vec::new();
            while let Some(slot) = list.pop() {
                if let Some((_, label, teardown, disposer)) = slot {
                    disposers.push((label, teardown, disposer));
                }
            }
            disposers
        };
        // Vec::pop yields reverse insertion order: exactly LIFO.
        for (label, teardown, disposer) in disposers {
            self.run_one_disposer(label, teardown, disposer).await;
            self.bump_settle();
        }
    }

    /// Snapshot of live effect labels, registration order (diagnostics).
    #[must_use]
    pub fn effect_metas(&self) -> Vec<String> {
        self.disposables
            .lock()
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(_, label, _, _)| label.clone()))
            .collect()
    }

    /// Number of live effects.
    #[must_use]
    pub fn effect_count(&self) -> usize {
        self.disposables
            .lock()
            .iter()
            .filter(|slot| slot.is_some())
            .count()
    }
}

// ---------------------------------------------------------------------------
// Reactive convergence
// ---------------------------------------------------------------------------

impl Fiber {
    /// Re-evaluates the desired epoch from committed candidates and requests
    /// convergence (TS `_refresh`).
    ///
    /// Epoch grammar: `":"` marks an activatable fiber; every satisfied
    /// dependency appends `":<provider-uid>"`. Any missing dependency
    /// collapses the epoch to [`EPOCH_INACTIVE`].
    pub(crate) fn refresh(self: &Arc<Self>) {
        let candidates = self.candidates.lock();
        let mut epoch = String::from(":");
        for injection in &self.inject {
            if let Some(snapshot) = candidates.get(injection.service.as_ref()) {
                epoch.push(':');
                epoch.push_str(&snapshot.provider_uid.to_string());
            } else {
                epoch = EPOCH_INACTIVE.to_string();
                break;
            }
        }
        drop(candidates);
        self.request_epoch(epoch);
    }

    /// Updates one candidate snapshot after a provide/withdraw notification.
    /// Returns `true` when the candidate set changed.
    pub(crate) fn check_impl(&self, name: &str) -> bool {
        let resolved = resolve_impl_for(&self.ctx(), self, name);
        let mut candidates = self.candidates.lock();
        match resolved {
            Some(snapshot) => {
                let changed = candidates
                    .get(name)
                    .is_none_or(|existing| existing.provider_uid != snapshot.provider_uid);
                candidates.insert(name.to_string(), snapshot);
                changed
            }
            None => candidates.remove(name).is_some(),
        }
    }

    /// Commits `name -> this fiber` into the committed view so the provider
    /// can read its own service during activation (TS does the same in
    /// `provide`).
    pub(crate) fn commit_self(&self, name: &str, snapshot: ProviderSnapshot) {
        self.store.lock().insert(name.to_string(), snapshot);
    }

    fn request_epoch(self: &Arc<Self>, desired: String) {
        let mut driver = self.driver.lock();
        if driver.desired == desired && !driver.dirty {
            return;
        }
        if driver.desired != desired {
            // A new convergence wave gets a fresh apply-retry budget (S3).
            driver.retries_used = 0;
        }
        driver.desired = desired;
        driver.dirty = true;
        if driver
            .task
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
        {
            driver.task = Some(tokio::spawn(Self::drive_loop(Arc::clone(self))));
        }
        drop(driver);
        self.cache_state();
        self.bump_settle();
        self.publish_state();
    }

    /// Convergence loop: serialized transitions per fiber (decision D4).
    async fn drive_loop(self: Arc<Self>) {
        loop {
            let (target, force) = {
                let mut driver = self.driver.lock();
                driver.dirty = false;
                (driver.desired.clone(), driver.force)
            };
            let loaded = self.loaded.lock().clone();
            if target == loaded && !force {
                let mut driver = self.driver.lock();
                if driver.dirty {
                    continue;
                }
                driver.task = None;
                let desired = driver.desired.clone();
                drop(driver);
                // `loaded` is written only by this task and `desired` was
                // captured under the driver lock: one atomic landing snapshot.
                invariant::settled(&self, &loaded, &desired);
                self.cache_state();
                self.bump_settle();
                self.publish_state();
                return;
            }
            if loaded != EPOCH_INACTIVE {
                self.unload_once().await;
                if force {
                    let mut driver = self.driver.lock();
                    driver.force = false;
                }
                continue;
            }
            if target == EPOCH_INACTIVE {
                let mut driver = self.driver.lock();
                driver.force = false;
                drop(driver);
                self.cache_state();
                continue;
            }
            // Stale-target pre-check (TS: never run plugin code for a stale
            // epoch).
            if self.driver.lock().desired != target {
                continue;
            }
            if matches!(self.load_once(&target).await, LoadOutcome::TimedOut) {
                /// What the driver does after a body timeout.
                enum After {
                    /// Re-run the same target within the retry budget.
                    Retry,
                    /// Park `Failed` with the timeout error.
                    Park,
                    /// A newer epoch owns the driver now; loop on and let the
                    /// next iteration converge on it (parking would clobber
                    /// the new desired epoch).
                    Stale,
                }
                let after = {
                    let policy = self
                        .ctx()
                        .root
                        .timeouts
                        .apply
                        .map_or(crate::timeout::ApplyPolicy::Fail, |apply| apply.policy);
                    let mut driver = self.driver.lock();
                    if driver.desired != target {
                        After::Stale
                    } else if let crate::timeout::ApplyPolicy::Retry(max) = policy
                        && driver.retries_used < max
                    {
                        driver.retries_used += 1;
                        After::Retry
                    } else {
                        After::Park
                    }
                };
                match after {
                    After::Retry | After::Stale => {}
                    After::Park => self.fail_load(Error::apply_timeout(&self.name())),
                }
            }
        }
    }

    async fn unload_once(self: &Arc<Self>) {
        invariant::pre_unload(self);
        self.publish_state();
        // Reap the activation's task scope before unwinding effects (S2): no
        // listener invocation may outlive the teardown below — a dylib
        // post-teardown call is documented UB. Abort-safe tasks drop at
        // their next yield point; tracked boundary calls run to completion.
        let scope = self.current_scope();
        self.reap_scope(&scope).await;
        // Drop committed edges before draining so `relied()` observes the
        // withdrawal as early as possible.
        {
            let root = self.ctx().root;
            let mut index = root.index.lock();
            index.uncommit(self.uid_value().unwrap_or(0));
        };
        self.unload_effects().await;
        self.store.lock().clear();
        *self.loaded.lock() = EPOCH_INACTIVE.to_string();
        self.cache_state();
        self.bump_settle();
        self.publish_state();
        invariant::post_unload(self);
    }

    /// Cancels the scope and reaps it (S2), surfacing a tracked task that
    /// outlives the reap-watchdog budget with an error log per interval
    /// (S3). Never proceeds past a non-empty scope: tearing down while a
    /// tracked foreign call is in flight is documented UB for dylib
    /// instances, so the watchdog only ever reports.
    async fn reap_scope(&self, scope: &Arc<CancelScope>) {
        let Some(budget) = self.ctx().root.timeouts.reap_watchdog else {
            scope.cancel_and_reap().await;
            return;
        };
        scope.cancel();
        let reap = scope.reap();
        tokio::pin!(reap);
        loop {
            tokio::select! {
                biased;
                () = &mut reap => break,
                () = tokio::time::sleep(budget) => {
                    self.ctx().logger().log(
                        LogLevel::Error,
                        "dougong",
                        &format!(
                            "reap_blocked: fiber \"{}\" (uid {:?}) still waits on {} tracked task(s) after {budget:?}",
                            self.name(),
                            self.uid_value(),
                            scope.task_count(),
                        ),
                    );
                }
            }
        }
    }

    /// Resolves this activation's config: the `internal/config` waterfall,
    /// then JSON-shape validation. The whole listener chain is bounded by the
    /// listener budget (S3): internal events reject bridges, so every
    /// listener here is plain in-process code and dropping the chain at a
    /// poll boundary is abort-safe.
    async fn resolve_config(self: &Arc<Self>) -> Result<Value> {
        let raw = self.raw_config.lock().clone();
        let ctx = self.ctx();
        let resolve = crate::events::waterfall_internal(
            &ctx,
            "internal/config",
            Some(Arc::clone(self)),
            raw,
            |value| Box::pin(async move { Ok(value) }),
        );
        let resolved = match ctx.root.timeouts.listener {
            Some(budget) => match tokio::time::timeout(budget, resolve).await {
                Ok(resolved) => resolved,
                Err(_elapsed) => return Err(Error::listener_timeout("internal/config")),
            },
            None => resolve.await,
        };
        let value = resolved?;
        crate::error::require_object(&value)?;
        Ok(value)
    }

    /// Commits the candidate snapshots as the new committed view ω BEFORE the
    /// body runs: the plugin reads its dependencies through this view during
    /// activation (TS `_reload` snapshots `this.store = {...this._store}`
    /// before executing the callback). Returns the committed map for the
    /// post-load checkpoint.
    fn commit_view(&self) -> HashMap<String, ProviderSnapshot> {
        let committed: HashMap<String, ProviderSnapshot> = self.candidates.lock().clone();
        let root = self.ctx().root;
        let mut edges: HashMap<String, (u64, u64)> = HashMap::new();
        let scope = Arc::clone(&self.scope);
        for (name, snapshot) in &committed {
            let label = ScopeNode::effective_label(Some(&scope), name);
            edges.insert(name.clone(), (label, snapshot.provider_uid));
        }
        root.index
            .lock()
            .commit_edges(self.uid_value().unwrap_or(0), &edges);
        self.store.lock().clone_from(&committed);
        committed
    }

    async fn load_once(self: &Arc<Self>, target: &str) -> LoadOutcome {
        invariant::pre_load(self, target);
        self.publish_state();
        let Some(runtime) = self.runtime.as_ref().map(Arc::clone) else {
            *self.loaded.lock() = target.to_string();
            return LoadOutcome::Landed;
        };
        let config = match self.resolve_config().await {
            Ok(config) => config,
            Err(err) => {
                self.fail_load(err);
                return LoadOutcome::Landed;
            }
        };
        *self.config.lock() = config.clone();
        let committed = self.commit_view();

        // A panicking plugin body must not kill the driver task (that would
        // hang every `join`): the body runs on its own task inside the
        // freshly minted scope generation, and a panic is reported through
        // the JoinHandle as a startup failure so the rollback path runs
        // (self-evolving runtimes host generated code, paper §1.2).
        let scope = self.reset_scope();
        let body_ctx = self.ctx();
        let body = runtime.plugin.apply(body_ctx.clone(), config.clone());
        let spawned = if runtime.plugin.body_abort_safe() {
            scope.spawn(body)
        } else {
            // A boundary body holds its foreign execution context across
            // awaits (a wasmtime store forbids abandoning an in-flight
            // call), so it is never dropped mid-flight; the sandbox budget
            // bounds the wait instead.
            scope.spawn_tracked(body)
        };
        // `None`: the scope was cancelled before the body started (a dispose
        // raced this load) — the cancellation arm below handles it.
        let outcome = match spawned {
            Some(handle) => self.await_body(handle, &scope).await,
            None => BodyOutcome::Cancelled,
        };
        match outcome {
            BodyOutcome::Completed(Ok(())) => {
                self.error.lock().take();
                *self.loaded.lock() = target.to_string();
                // A successful load starts a fresh retry budget (S3).
                self.driver.lock().retries_used = 0;
                // Republish state BEFORE notifying dependents: their
                // `check_impl` consults provider liveness.
                self.cache_state();
                let root = self.ctx().root;
                let provided = {
                    let store = root.reflect.lock();
                    store.provided_names(self.uid_value().unwrap_or(0))
                };
                crate::service::notify(&self.ctx(), &provided);
                self.bump_settle();
                self.publish_state();
                invariant::post_load(self, target, &committed);
                LoadOutcome::Landed
            }
            BodyOutcome::Completed(Err(err)) => {
                // Transactional activation: roll back what the body collected
                // and what was committed above.
                self.rollback_activation(&scope).await;
                self.fail_load(err);
                LoadOutcome::Landed
            }
            BodyOutcome::Cancelled => {
                // The scope was cancelled (a dispose raced the load): the
                // body was aborted at a yield point or never started.
                self.rollback_activation(&scope).await;
                self.fail_load(Error::msg("plugin task was cancelled"));
                LoadOutcome::Landed
            }
            BodyOutcome::Panicked(join_error) => {
                let err = if join_error.is_panic() {
                    let payload = join_error.into_panic();
                    let text = payload
                        .downcast_ref::<&str>()
                        .map(|slice| (*slice).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "plugin body panicked".to_string());
                    Error::msg(format!("plugin panicked: {text}"))
                } else {
                    Error::msg("plugin task was cancelled")
                };
                self.rollback_activation(&scope).await;
                self.fail_load(err);
                LoadOutcome::Landed
            }
            BodyOutcome::TimedOut => {
                // Rolled back here; the driver decides between retrying the
                // same target and parking `Failed` (S3).
                self.rollback_activation(&scope).await;
                LoadOutcome::TimedOut
            }
        }
    }

    /// Awaits the spawned body task under the apply budget (S3).
    ///
    /// On timeout the scope is cancelled and the task is still awaited to
    /// settlement: an abort-safe body resolves `None` at its next yield
    /// point, while a tracked boundary body runs its foreign call to
    /// completion (the wasm sandbox budget bounds that wait; a synchronous
    /// FFI loop that never returns is surfaced by the reap watchdog). The
    /// timeout policy owns the outcome either way, so a late body result —
    /// including a late success — is discarded; a late panic is logged.
    async fn await_body(
        &self,
        mut handle: tokio::task::JoinHandle<Option<Result<()>>>,
        scope: &Arc<CancelScope>,
    ) -> BodyOutcome {
        fn classify(result: Result<Option<Result<()>>, tokio::task::JoinError>) -> BodyOutcome {
            match result {
                Ok(Some(result)) => BodyOutcome::Completed(result),
                Ok(None) => BodyOutcome::Cancelled,
                Err(join_error) => BodyOutcome::Panicked(join_error),
            }
        }
        let Some(apply) = self.ctx().root.timeouts.apply else {
            return classify(handle.await);
        };
        tokio::select! {
            biased;
            result = &mut handle => classify(result),
            () = tokio::time::sleep(apply.budget) => {
                scope.cancel();
                if let Err(join_error) = handle.await
                    && join_error.is_panic()
                {
                    self.ctx().logger().log(
                        LogLevel::Error,
                        "dougong",
                        &format!(
                            "apply_timeout: fiber \"{}\" (uid {:?}) body panicked while settling after its budget fired: {join_error}",
                            self.name(),
                            self.uid_value(),
                        ),
                    );
                }
                BodyOutcome::TimedOut
            }
        }
    }

    /// Transactional activation rollback: reap the scope generation first
    /// (no task — and therefore no guest call — may outlive the teardown
    /// effects), then drain collected effects LIFO, uncommit the index
    /// edges, and clear the store. Parking the fiber with the startup error
    /// is the caller's call (the S3 retry policy needs the split).
    async fn rollback_activation(self: &Arc<Self>, scope: &Arc<CancelScope>) {
        self.reap_scope(scope).await;
        self.unload_effects().await;
        {
            let root = self.ctx().root;
            let mut index = root.index.lock();
            index.uncommit(self.uid_value().unwrap_or(0));
        }
        self.store.lock().clear();
    }

    fn fail_load(self: &Arc<Self>, err: Error) {
        let ctx = self.ctx();
        ctx.logger().log_error(&err);
        self.error.lock().replace(err);
        *self.loaded.lock() = EPOCH_INACTIVE.to_string();
        // Park: the next provide/withdraw notification re-arms convergence.
        let mut driver = self.driver.lock();
        driver.desired = EPOCH_INACTIVE.to_string();
        drop(driver);
        self.cache_state();
        self.bump_settle();
        self.publish_state();
        invariant::post_fail_load(self);
    }
}

// ---------------------------------------------------------------------------
// Public controls
// ---------------------------------------------------------------------------

impl Fiber {
    /// Waits until the fiber settles into a stable state; surfaces startup
    /// errors.
    ///
    /// # Errors
    ///
    /// Rethrows the startup error, if any.
    pub async fn join(&self) -> Result<()> {
        let mut rx = self.settle.1.clone();
        loop {
            let busy = {
                let driver = self.driver.lock();
                driver.dirty
                    || driver.task.as_ref().is_some_and(|task| !task.is_finished())
                    || *self.loaded.lock() != driver.desired
            };
            if !busy {
                break;
            }
            let _ = *rx.borrow_and_update();
            if rx.changed().await.is_err() {
                break;
            }
        }
        self.error
            .lock()
            .as_ref()
            .map_or(Ok(()), |err| Err(err.clone()))
    }

    /// Like [`Fiber::join`], but gives up after `timeout` while the driver
    /// keeps converging in the background.
    ///
    /// This bounds the wait when a plugin body never completes, so callers can
    /// decide whether to keep waiting, restart, or dispose.
    ///
    /// # Errors
    ///
    /// Returns a timeout error when the deadline elapses; rethrows startup
    /// errors otherwise.
    pub async fn join_with_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        let mut rx = self.settle.1.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let busy = {
                let driver = self.driver.lock();
                driver.dirty
                    || driver.task.as_ref().is_some_and(|task| !task.is_finished())
                    || *self.loaded.lock() != driver.desired
            };
            if !busy {
                break;
            }
            let _ = *rx.borrow_and_update();
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::msg(format!("join(\"{}\") timed out", self.name())));
            }
            if tokio::time::timeout(remaining, rx.changed()).await.is_err() {
                return Err(Error::msg(format!("join(\"{}\") timed out", self.name())));
            }
        }
        self.error
            .lock()
            .as_ref()
            .map_or(Ok(()), |err| Err(err.clone()))
    }

    /// Disposes and immediately reloads this plugin with its current config.
    ///
    /// # Errors
    ///
    /// Fails on disposed fibers; rethrows startup errors after the round trip.
    pub async fn restart(self: &Arc<Self>) -> Result<()> {
        self.assert_active()?;
        self.error.lock().take();
        {
            let mut driver = self.driver.lock();
            // A fresh unload->load round trip is only needed when something is
            // currently loaded; a Failed/Pending fiber just loads directly.
            driver.force = self.loaded.lock().as_str() != EPOCH_INACTIVE;
            driver.dirty = true;
            // An explicit restart is operator action: fresh retry budget (S3).
            driver.retries_used = 0;
            if driver
                .task
                .as_ref()
                .is_none_or(tokio::task::JoinHandle::is_finished)
            {
                driver.task = Some(tokio::spawn(Self::drive_loop(Arc::clone(self))));
            }
        }
        self.bump_settle();
        self.refresh();
        self.join().await
    }

    /// Validates and applies new config, then restarts the plugin.
    ///
    /// Runs the `internal/update` waterfall first: an update hook that never
    /// calls `next` vetoes the change (HMR extension point).
    ///
    /// # Errors
    ///
    /// Fails on disposed fibers, waterfall failures, or startup errors.
    pub async fn update(self: &Arc<Self>, config: Value) -> Result<Value> {
        self.assert_active()?;
        if self.state() != FiberState::Active {
            self.error.lock().take();
            *self.raw_config.lock() = config;
            self.request_epoch(crate::fiber::EPOCH_INACTIVE.to_string());
            self.refresh();
            return Ok(Value::Null);
        }
        let fiber = Arc::clone(self);
        crate::events::waterfall_internal(
            &self.ctx(),
            "internal/update",
            Some(Arc::clone(self)),
            config,
            move |config| {
                let fiber = Arc::clone(&fiber);
                Box::pin(async move {
                    // Persist the config only in the tail: a hook that vetoes
                    // (never pulls `next`) must leave the previous config live
                    // for the next restart.
                    *fiber.raw_config.lock() = config;
                    fiber.restart().await?;
                    Ok(Value::Null)
                })
            },
        )
        .await?;
        Ok(self.config.lock().clone())
    }

    /// Disposes this fiber: unload the plugin, settle once cleanup finished.
    ///
    /// Idempotent; a second call is a no-op.
    pub async fn dispose(self: &Arc<Self>) {
        let old_uid = self.uid_value();
        let Some(old_uid) = old_uid else {
            return;
        };
        *self.uid.lock() = None;
        self.cache_state();
        if old_uid != 0 {
            crate::registry::remove_from_runtime(self);
            let inject_names = self.inject_names();
            self.ctx().root.remove_fiber(old_uid, &inject_names);
        }
        let payload = serde_json::json!({
            "event": "disposed",
            "fiber": self.name(),
            "uid": old_uid,
        });
        events::emit_internal(&self.ctx(), "internal/plugin", &payload, None);
        // Cancel the task scope up front (S2): a hung in-flight body would
        // otherwise park the driver — and this join — forever. The reap runs
        // on the driver path (`unload_once` / the load rollback arms), which
        // also keeps this safe when dispose is invoked from one of this
        // fiber's own listener tasks (no self-join).
        self.current_scope().cancel();
        self.request_epoch(EPOCH_INACTIVE.to_string());
        let _ = self.join().await;
        self.publish_state();
        invariant::post_dispose(self, old_uid);
    }
}

/// Handle returned by `ctx.plugin()`: a shared pointer plus controls.
///
/// Cloning is cheap; dropping a handle never disposes the fiber. Call
/// [`FiberHandle::dispose`] or [`crate::Context::stop`] explicitly.
#[derive(Clone)]
pub struct FiberHandle(pub(crate) Arc<Fiber>);

impl std::fmt::Debug for FiberHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl FiberHandle {
    /// The fiber uid, or `None` when disposed.
    #[must_use]
    pub fn uid(&self) -> Option<u64> {
        *self.0.uid.lock()
    }

    /// The plugin display name.
    #[must_use]
    pub fn name(&self) -> String {
        self.0.name()
    }

    /// Current lifecycle state (lock-free read).
    #[must_use]
    pub fn state(&self) -> FiberState {
        self.0.state()
    }

    /// Number of live effects.
    #[must_use]
    pub fn effect_count(&self) -> usize {
        self.0.effect_count()
    }

    /// Labels of the live effects owned by this fiber.
    #[must_use]
    pub fn effect_metas(&self) -> Vec<String> {
        self.0.effect_metas()
    }

    /// Waits for convergence; rethrows startup errors.
    ///
    /// # Errors
    ///
    /// Rethrows the startup error, if any.
    pub async fn join(&self) -> Result<()> {
        self.0.join().await
    }

    /// Bounded [`FiberHandle::join`].
    ///
    /// # Errors
    ///
    /// Returns a timeout error when the deadline elapses.
    pub async fn join_with_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.0.join_with_timeout(timeout).await
    }

    /// Dispose and reload with the current config.
    ///
    /// # Errors
    ///
    /// Rethrows startup errors after the round trip.
    pub async fn restart(&self) -> Result<()> {
        self.0.restart().await
    }

    /// Applies new config and restarts (hot update path).
    ///
    /// # Errors
    ///
    /// Fails on disposed fibers, waterfall failures, or startup errors.
    pub async fn update(&self, config: Value) -> Result<Value> {
        self.0.update(config).await
    }

    /// Unloads the plugin and settles after cleanup finished.
    pub async fn dispose(&self) {
        self.0.dispose().await;
    }
}

/// Guard for eager single-effect disposal; disposing is single-shot.
#[derive(Clone)]
pub struct EffectGuard {
    fiber: Weak<Fiber>,
    id: u64,
}

impl std::fmt::Debug for EffectGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectGuard")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl EffectGuard {
    /// Disposes eagerly. Idempotent: a second call finds nothing to run.
    ///
    /// # Errors
    ///
    /// Never fails; disposer errors are logged and swallowed by the fiber.
    pub async fn dispose(&self) -> Result<()> {
        let Some(fiber) = self.fiber.upgrade() else {
            return Ok(());
        };
        fiber.run_disposer(self.id).await;
        Ok(())
    }

    /// True while the effect is still registered.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.fiber.upgrade().is_some_and(|fiber| {
            fiber
                .disposables
                .lock()
                .iter()
                .any(|slot| matches!(slot, Some((id, _, _, _)) if *id == self.id))
        })
    }
}
