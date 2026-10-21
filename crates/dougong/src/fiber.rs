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

use crate::context::Context;
use crate::error::{Error, Result};
use crate::events;
use crate::plugin::{BoxFuture, Injection, Plugin};
use crate::scope::ScopeNode;
use crate::service::resolve_impl_for;

/// A boxed async cleanup routine collected as a fiber effect.
pub type Disposer = Box<dyn FnOnce() -> BoxFuture<()> + Send>;

/// Sentinel epoch string meaning "required services are not all available".
pub const EPOCH_INACTIVE: &str = "";

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
    disposables: Mutex<Vec<Option<(u64, String, Disposer)>>>,
    next_effect_id: AtomicU64,
    driver: Mutex<DriveState>,
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
            }),
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
            }),
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
        disposer: Option<Disposer>,
    ) -> Result<EffectGuard> {
        self.ensure_can_effect()?;
        let effect_label = label.into();
        let id = self.next_effect_id.fetch_add(1, Ordering::SeqCst);
        if let Some(disposer) = disposer {
            let mut list = self.disposables.lock();
            list.push(Some((id, effect_label, disposer)));
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
        setup: impl std::future::Future<Output = Result<Option<Disposer>>> + Send,
    ) -> Result<EffectGuard> {
        self.ensure_can_effect()?;
        let effect_label = label.into();
        let disposer = setup.await?;
        self.register_effect(effect_label, disposer)
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
                .find(|slot| matches!(slot, Some((entry_id, _, _)) if *entry_id == id));
            let taken = slot
                .and_then(std::option::Option::take)
                .map(|(_, _, disposer)| disposer);
            drop(list);
            taken
        };
        if let Some(disposer) = taken {
            disposer().await;
            self.bump_settle();
        }
    }

    /// Drains every live effect LIFO, sequentially.
    async fn unload_effects(self: &Arc<Self>) {
        let disposers: Vec<Disposer> = {
            let mut list = self.disposables.lock();
            let mut disposers = Vec::new();
            while let Some(slot) = list.pop() {
                if let Some((_, _, disposer)) = slot {
                    disposers.push(disposer);
                }
            }
            disposers
        };
        // Vec::pop yields reverse insertion order: exactly LIFO.
        for disposer in disposers {
            disposer().await;
            self.bump_settle();
        }
    }

    /// Snapshot of live effect labels, registration order (diagnostics).
    #[must_use]
    pub fn effect_metas(&self) -> Vec<String> {
        self.disposables
            .lock()
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(_, label, _)| label.clone()))
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
                drop(driver);
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
            self.load_once(&target).await;
        }
    }

    async fn unload_once(self: &Arc<Self>) {
        self.publish_state();
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
    }

    async fn load_once(self: &Arc<Self>, target: &str) {
        self.publish_state();
        let Some(runtime) = self.runtime.as_ref().map(Arc::clone) else {
            *self.loaded.lock() = target.to_string();
            return;
        };

        // Config resolution: `internal/config` waterfall, then the plugin's
        // own validation (v1: JSON-shape only).
        let raw = self.raw_config.lock().clone();
        let resolved = crate::events::waterfall_internal(
            &self.ctx(),
            "internal/config",
            Some(Arc::clone(self)),
            raw,
            |value| Box::pin(async move { Ok(value) }),
        )
        .await;
        let config = match resolved {
            Ok(value) => {
                let check = crate::error::require_object(&value);
                match check {
                    Ok(()) => value,
                    Err(err) => return self.fail_load(err),
                }
            }
            Err(err) => return self.fail_load(err),
        };
        *self.config.lock() = config.clone();

        // Commit the candidate snapshots as the new committed view ω BEFORE
        // the body runs: the plugin reads its dependencies through this view
        // during activation (TS `_reload` snapshots `this.store = {...this._store}`
        // before executing the callback).
        let committed: HashMap<String, ProviderSnapshot> = self.candidates.lock().clone();
        {
            let root = self.ctx().root;
            let mut edges: HashMap<String, (u64, u64)> = HashMap::new();
            let scope = Arc::clone(&self.scope);
            for (name, snapshot) in &committed {
                let label = ScopeNode::effective_label(Some(&scope), name);
                edges.insert(name.clone(), (label, snapshot.provider_uid));
            }
            {
                let mut index = root.index.lock();
                index.commit_edges(self.uid_value().unwrap_or(0), &edges);
            }
            *self.store.lock() = committed;
        }

        // A panicking plugin body must not kill the driver task (that would
        // hang every `join`): the body runs on its own task, and a panic is
        // reported through the JoinHandle as a startup failure so the rollback
        // path runs (self-evolving runtimes host generated code, paper §1.2).
        let body_ctx = self.ctx();
        let body = runtime.plugin.apply(body_ctx.clone(), config.clone());
        let apply_outcome = tokio::spawn(body).await;
        match apply_outcome {
            Ok(Ok(())) => {
                self.error.lock().take();
                *self.loaded.lock() = target.to_string();
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
            }
            Ok(Err(err)) => {
                // Transactional activation: roll back what the body collected
                // and what was committed above.
                self.unload_effects().await;
                {
                    let root = self.ctx().root;
                    let mut index = root.index.lock();
                    index.uncommit(self.uid_value().unwrap_or(0));
                }
                self.store.lock().clear();
                self.fail_load(err);
            }
            Err(join_error) => {
                let message = if join_error.is_panic() {
                    let payload = join_error.into_panic();
                    let text = payload
                        .downcast_ref::<&str>()
                        .map(|slice| (*slice).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "plugin body panicked".to_string());
                    format!("plugin panicked: {text}")
                } else {
                    "plugin task was cancelled".to_string()
                };
                self.unload_effects().await;
                {
                    let root = self.ctx().root;
                    let mut index = root.index.lock();
                    index.uncommit(self.uid_value().unwrap_or(0));
                }
                self.store.lock().clear();
                self.fail_load(Error::msg(format!("plugin panicked: {message}")));
            }
        }
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
        self.request_epoch(EPOCH_INACTIVE.to_string());
        let _ = self.join().await;
        self.publish_state();
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
                .any(|slot| matches!(slot, Some((id, _, _)) if *id == self.id))
        })
    }
}
