// Rust guideline compliant 2026-10-20
//! Service store: typed provision over isolate-labeled slots, with the
//! reverse indexes required by the reactive model (design decision D12).
//!
//! Two indexes are maintained:
//!
//! - `key_interest`: service name -> fibers that declare an injection for
//!   it (registered at fiber creation). `notify` walks only candidates
//!   instead of scanning every fiber.
//! - `provider_consumers`: provider fiber uid -> consumer fiber uids whose
//!   committed views reference it. `relied()` is O(1); the drain protocol
//!   (provider withdrawal waits for consumers to converge) uses it.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::context::{Context, RootState};
use crate::error::{Error, Result};
use crate::fiber::{Fiber, FiberState, ProviderSnapshot};
use crate::scope::ScopeNode;

/// Availability predicate consulted before dependents may load.
pub type Check = Arc<dyn Fn(&Context) -> bool + Send + Sync>;

/// Computed (accessor) service body, evaluated per read.
pub type Compute = Arc<dyn Fn(&Context) -> Option<Arc<dyn Any + Send + Sync>> + Send + Sync>;

/// One registered implementation of a service in one isolation realm.
#[derive(Clone)]
pub struct Provided {
    /// Service name.
    pub name: String,
    /// The stored value (erased; consumers downcast).
    pub value: Arc<dyn Any + Send + Sync>,
    /// Fiber uid of the provider.
    pub provider_uid: u64,
    /// Optional availability predicate.
    pub check: Option<Check>,
    /// Optional computed-service body.
    pub compute: Option<Compute>,
}

impl std::fmt::Debug for Provided {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provided")
            .field("name", &self.name)
            .field("provider_uid", &self.provider_uid)
            .field("check", &self.check.is_some())
            .field("compute", &self.compute.is_some())
            .finish_non_exhaustive()
    }
}

/// Store of provided implementations, keyed by name then isolation label.
#[derive(Debug, Default)]
pub struct ReflectStore {
    labels: HashMap<String, HashMap<u64, Provided>>,
}

impl ReflectStore {
    /// Inserts an implementation; fails when the slot is occupied.
    pub(crate) fn insert(&mut self, provided: Provided, label: u64, owner: &str) -> Result<()> {
        let bucket = self.labels.entry(provided.name.clone()).or_default();
        if bucket.contains_key(&label) {
            return Err(Error::duplicate_provide(&provided.name, owner));
        }
        bucket.insert(label, provided);
        Ok(())
    }

    /// Seeds a built-in implementation during context construction, bypassing
    /// the duplicate check.
    ///
    /// # Panics
    ///
    /// Panics when the slot is occupied: seeding happens only at construction
    /// time on an empty store, so occupation indicates a programming error.
    pub(crate) fn seed(&mut self, provided: Provided, label: u64) {
        let previous = self
            .labels
            .entry(provided.name.clone())
            .or_default()
            .insert(label, provided);
        debug_assert!(previous.is_none(), "builtin slot seeded twice");
    }

    /// Removes and returns the implementation at `(name, label)`.
    pub(crate) fn remove(&mut self, name: &str, label: u64) -> Option<Provided> {
        self.labels.get_mut(name)?.remove(&label)
    }

    /// Mutable access to one implementation slot.
    pub(crate) fn get_mut_entry(&mut self, name: &str, label: u64) -> Option<&mut Provided> {
        self.labels.get_mut(name)?.get_mut(&label)
    }

    /// Resolves the implementation for `(name, label)` under `strict` mode.
    ///
    /// Strict mode only returns implementations whose providing fiber is
    /// currently `Active` and whose availability predicate passes.
    pub(crate) fn get_impl(
        &self,
        root: &RootState,
        name: &str,
        label: u64,
        strict: bool,
    ) -> Option<Provided> {
        let provided = self.labels.get(name)?.get(&label)?;
        if strict {
            if !root.fiber_is_active(provided.provider_uid) {
                return None;
            }
            if let Some(check) = &provided.check {
                let provider_ctx = root.context_of(provided.provider_uid)?;
                if !check(&provider_ctx) {
                    return None;
                }
            }
        }
        Some(provided.clone())
    }

    /// Names of services currently provided by fiber `uid`.
    pub(crate) fn provided_names(&self, uid: u64) -> Vec<String> {
        let mut names = Vec::new();
        for (name, bucket) in &self.labels {
            if bucket.values().any(|provided| provided.provider_uid == uid) {
                names.push(name.clone());
            }
        }
        names
    }
}

/// Reverse indexes maintained by the kernel (D12).
#[derive(Debug, Default)]
pub struct DependencyIndex {
    /// Service name -> uids of fibers that declare an injection for it.
    pub(crate) key_interest: HashMap<String, HashSet<u64>>,
    /// Provider uid -> uids of consumers whose committed views reference it.
    pub(crate) provider_consumers: HashMap<u64, HashSet<u64>>,
    /// Consumer uid -> provider uids it currently depends on (cleanup mirror
    /// of `provider_consumers`).
    pub(crate) consumer_providers: HashMap<u64, HashSet<u64>>,
}

impl DependencyIndex {
    /// Registers a fiber's declared interest in its injected service names.
    pub(crate) fn add_interest(&mut self, uid: u64, names: &[String]) {
        for name in names {
            self.key_interest
                .entry(name.clone())
                .or_default()
                .insert(uid);
        }
    }

    /// Drops all index entries for a disposed fiber.
    pub(crate) fn remove_fiber(&mut self, uid: u64, interested: &[String]) {
        for name in interested {
            if let Some(set) = self.key_interest.get_mut(name) {
                set.remove(&uid);
                if set.is_empty() {
                    self.key_interest.remove(name);
                }
            }
        }
        if let Some(providers) = self.consumer_providers.remove(&uid) {
            for provider in providers {
                if let Some(set) = self.provider_consumers.get_mut(&provider) {
                    set.remove(&uid);
                    if set.is_empty() {
                        self.provider_consumers.remove(&provider);
                    }
                }
            }
        }
    }

    /// Records the dependency edges of a fiber's committed view.
    pub(crate) fn commit_edges(&mut self, uid: u64, providers: &HashMap<String, (u64, u64)>) {
        // providers: service name -> (label, provider uid)
        for (label, provider) in providers.values() {
            let _ = label;
            self.provider_consumers
                .entry(*provider)
                .or_default()
                .insert(uid);
            self.consumer_providers
                .entry(uid)
                .or_default()
                .insert(*provider);
        }
    }

    /// Drops a fiber's committed edges before its store is cleared.
    pub(crate) fn uncommit(&mut self, uid: u64) {
        if let Some(providers) = self.consumer_providers.remove(&uid) {
            for provider in providers {
                if let Some(set) = self.provider_consumers.get_mut(&provider) {
                    set.remove(&uid);
                    if set.is_empty() {
                        self.provider_consumers.remove(&provider);
                    }
                }
            }
        }
    }

    /// True when at least one committed consumer references `provider_uid`.
    #[must_use]
    pub fn relied(&self, provider_uid: u64) -> bool {
        self.provider_consumers
            .get(&provider_uid)
            .is_some_and(|set| !set.is_empty())
    }
}

/// Resolves the implementation snapshot a fiber would commit for `name`.
///
/// Returns `None` when the service is absent, the provider is inactive
/// (strict), or the availability predicate fails.
pub fn resolve_impl_for(ctx: &Context, dependent: &Fiber, name: &str) -> Option<ProviderSnapshot> {
    let root = &ctx.root;
    let label = ScopeNode::effective_label(Some(dependent.scope_node()), name);
    let provided = {
        let store = root.reflect.lock();
        store.get_impl(root, name, label, true)?
    };
    if provided.compute.is_some() {
        // Accessor entries commit only liveness metadata; values are computed
        // per read against the reading context.
        return Some(ProviderSnapshot::dynamic(provided.provider_uid));
    }
    Some(ProviderSnapshot {
        provider_uid: provided.provider_uid,
        value: Arc::clone(&provided.value),
        dynamic: false,
    })
}

/// Registers a provided service owned by the current fiber.
///
/// The returned disposer performs the withdrawal protocol: remove the
/// implementation, wake dependents, and wait for every refreshed fiber to
/// converge before returning (paper Thm 63, TS provide-disposer drain).
///
/// # Errors
///
/// Fails when no fiber is bound, the fiber cannot accept effects, or the
/// `(name, realm)` slot is already occupied.
pub async fn provide(
    ctx: &Context,
    name: &str,
    value: Arc<dyn Any + Send + Sync>,
    check: Option<Check>,
) -> Result<crate::fiber::EffectGuard> {
    let fiber = ctx.require_fiber("provide")?;
    let label = ctx.effective_label(name);
    let owner = fiber.name();
    let uid = fiber.uid_value().ok_or_else(Error::inactive_effect)?;
    let self_value = Arc::clone(&value);
    let provided = Provided {
        name: name.to_string(),
        value,
        provider_uid: uid,
        check,
        compute: None,
    };
    let name_owned = name.to_string();
    let effect_ctx = ctx.clone();
    let fiber_in = Arc::clone(&fiber);
    fiber
        .effect(format!("ctx.provide({name:?})"), async move {
            {
                let mut store = effect_ctx.root.reflect.lock();
                store.insert(provided, label, &owner)?;
            };
            // The provider must see itself during activation (TS writes
            // the impl into its own committed view).
            fiber_in.commit_self(
                &name_owned,
                crate::fiber::ProviderSnapshot {
                    provider_uid: uid,
                    value: self_value,
                    dynamic: false,
                },
            );
            if fiber_in.state() == FiberState::Active {
                notify(&effect_ctx, std::slice::from_ref(&name_owned));
            }
            let disposer_name = name_owned.clone();
            let disposer_ctx = effect_ctx.clone();
            let disposer: crate::fiber::Disposer = Box::new(move || {
                let name = disposer_name.clone();
                let ctx = disposer_ctx.clone();
                Box::pin(async move {
                    {
                        let mut store = ctx.root.reflect.lock();
                        store.remove(&name, label)
                    };
                    // Withdrawal protocol: wake dependents, then wait for
                    // them to converge before returning. Consumer join
                    // errors are the consumer's own startup failures, already
                    // logged there; the drain only needs convergence.
                    let touched = notify(&ctx, &[name]);
                    for fiber in touched {
                        let _ = fiber.join().await;
                    }
                })
            });
            Ok(Some(disposer))
        })
        .await
}

/// Registers a computed (accessor) service owned by the current fiber.
///
/// # Errors
///
/// Same conditions as [`provide`].
pub async fn accessor(
    ctx: &Context,
    name: &str,
    compute: Compute,
) -> Result<crate::fiber::EffectGuard> {
    let fiber = ctx.require_fiber("accessor")?;
    let label = ctx.effective_label(name);
    let owner = fiber.name();
    let uid = fiber.uid_value().ok_or_else(Error::inactive_effect)?;
    let provided = Provided {
        name: name.to_string(),
        value: Arc::new(()),
        provider_uid: uid,
        check: None,
        compute: Some(compute),
    };
    let name_owned = name.to_string();
    let effect_ctx = ctx.clone();
    let fiber_in = Arc::clone(&fiber);
    fiber
        .effect(format!("ctx.accessor({name:?})"), async move {
            {
                let mut store = effect_ctx.root.reflect.lock();
                store.insert(provided, label, &owner)?;
            };
            fiber_in.commit_self(&name_owned, crate::fiber::ProviderSnapshot::dynamic(uid));
            if fiber_in.state() == FiberState::Active {
                notify(&effect_ctx, std::slice::from_ref(&name_owned));
            }
            let disposer_ctx = effect_ctx.clone();
            let disposer_name = name_owned.clone();
            let disposer: crate::fiber::Disposer = Box::new(move || {
                let name = disposer_name.clone();
                let ctx = disposer_ctx.clone();
                Box::pin(async move {
                    ctx.root.reflect.lock().remove(&name, label);
                    let touched = notify(&ctx, &[name]);
                    for fiber in touched {
                        let _ = fiber.join().await;
                    }
                })
            });
            Ok(Some(disposer))
        })
        .await
}

/// Overwrites the value of a service provided by the current fiber.
///
/// # Errors
///
/// Fails when the service is not provided by this fiber.
pub fn set(ctx: &Context, name: &str, value: Arc<dyn Any + Send + Sync>) -> Result<()> {
    let fiber = ctx.fiber().ok_or_else(Error::inactive_effect)?;
    let label = ctx.effective_label(name);
    let uid = fiber.uid_value().ok_or_else(Error::inactive_effect)?;
    {
        let mut store = ctx.root.reflect.lock();
        let impl_ = store
            .get_mut_entry(name, label)
            .ok_or_else(|| Error::undeclared_dependency(name))?;
        if impl_.provider_uid != uid {
            return Err(Error::not_provider(name));
        }
        impl_.value = value;
        drop(store);
    }
    Ok(())
}

/// Re-evaluates every interested fiber for the given service names and emits
/// `internal/service` once per name.
///
/// Returns the fibers whose dependency state changed (the withdrawal protocol
/// awaits them).
pub fn notify(ctx: &Context, names: &[String]) -> Vec<Arc<Fiber>> {
    let mut touched = Vec::new();
    {
        let index = ctx.root.index.lock();
        let mut candidates: Vec<u64> = Vec::new();
        for name in names {
            if let Some(set) = index.key_interest.get(name) {
                candidates.extend(set.iter().copied());
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        drop(index);
        let fibers = ctx.root.fibers.lock();
        for uid in candidates {
            if let Some(weak) = fibers.get(&uid)
                && let Some(fiber) = weak.upgrade()
            {
                let mut changed = false;
                for name in names {
                    if fiber.declares(name) {
                        changed |= fiber.check_impl(name);
                    }
                }
                if changed {
                    fiber.refresh();
                    touched.push(fiber);
                }
            }
        }
    }
    for name in names {
        let value = {
            let store = ctx.root.reflect.lock();
            store
                .get_impl(&ctx.root, name, ctx.effective_label(name), false)
                .map(|p| p.value)
        };
        let payload = serde_json::json!({ "service": name, "provided": value.is_some() });
        // Service events are scope-filtered: listeners registered under a
        // different isolation label for this service do not receive it.
        let emitter_label = ctx.effective_label(name);
        let filter = |hook_ctx: &Context| hook_ctx.effective_label(name) == emitter_label;
        crate::events::emit_internal(ctx, "internal/service", &payload, Some(&filter));
    }
    touched
}

/// Merged intercept config for `name` visible from `scope` (root-most first).
#[must_use]
pub fn merged_intercept(scope: &Arc<ScopeNode>, name: &str) -> Value {
    let layers = ScopeNode::intercept_layers(Some(scope), name);
    let mut merged = serde_json::Map::new();
    for layer in layers {
        if let Value::Object(map) = layer {
            for (key, value) in map {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(merged)
}
