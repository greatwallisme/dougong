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
use crate::logger::{LogLevel, LogRecord};
use crate::scope::ScopeNode;
use crate::shard::ShardMap;

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

/// Store of provided implementations, striped by service name (P1); each
/// stripe keeps the `name -> label -> Provided` buckets of its names.
///
/// The strict-resolution checks (provider liveness, availability predicate)
/// run *after* the stripe lock is released: they touch the active set and
/// the fiber table, and no store stripe may be held across those locks (the
/// root-state lock-order rule — see `shard.rs`).
#[derive(Debug, Default)]
pub struct ReflectStore {
    slots: ShardMap<String, HashMap<u64, Provided>>,
}

impl ReflectStore {
    /// Inserts an implementation; fails when the slot is occupied.
    pub(crate) fn insert(&self, provided: Provided, label: u64, owner: &str) -> Result<()> {
        // The striping borrow and the closure both need the name: one extra
        // clone on a registration path (cold) keeps the shared-key shape.
        let name = provided.name.clone();
        self.slots.write(&name.clone(), |slots| {
            let bucket = slots.entry(name).or_default();
            if bucket.contains_key(&label) {
                return Err(Error::duplicate_provide(&provided.name, owner));
            }
            bucket.insert(label, provided);
            Ok(())
        })
    }

    /// Seeds a built-in implementation during context construction, bypassing
    /// the duplicate check.
    ///
    /// # Panics
    ///
    /// Panics when the slot is occupied: seeding happens only at construction
    /// time on an empty store, so occupation indicates a programming error.
    pub(crate) fn seed(&self, provided: Provided, label: u64) {
        let name = provided.name.clone();
        let previous = self.slots.write(&name.clone(), |slots| {
            slots.entry(name).or_default().insert(label, provided)
        });
        debug_assert!(previous.is_none(), "builtin slot seeded twice");
    }

    /// Removes and returns the implementation at `(name, label)`.
    pub(crate) fn remove(&self, name: &str, label: u64) -> Option<Provided> {
        self.slots
            .write(name, |slots| slots.get_mut(name)?.remove(&label))
    }

    /// Overwrites the value of the implementation at `(name, label)`; fails
    /// unless the slot exists and is owned by fiber `uid` (the `Context::set`
    /// path).
    pub(crate) fn overwrite(
        &self,
        name: &str,
        label: u64,
        uid: u64,
        value: Arc<dyn Any + Send + Sync>,
    ) -> Result<()> {
        self.slots.write(name, |slots| {
            let entry = slots
                .get_mut(name)
                .and_then(|bucket| bucket.get_mut(&label))
                .ok_or_else(|| Error::undeclared_dependency(name))?;
            if entry.provider_uid != uid {
                return Err(Error::not_provider(name));
            }
            entry.value = value;
            Ok(())
        })
    }

    /// Resolves the implementation for `(name, label)` under `strict` mode.
    ///
    /// Strict mode only returns implementations whose providing fiber is
    /// currently `Active` and whose availability predicate passes. The entry
    /// is cloned under the stripe lock and the strict checks run after the
    /// lock is released (lock-order rule); the checks were never atomic with
    /// the store read even under the pre-P1 single lock — the active set and
    /// the fiber table were always separate locks.
    pub(crate) fn get_impl(
        &self,
        root: &RootState,
        name: &str,
        label: u64,
        strict: bool,
    ) -> Option<Provided> {
        let provided = self.slots.read(name, |slots| slots?.get(&label).cloned())?;
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
        Some(provided)
    }

    /// Names of services currently provided by fiber `uid`. Scans every
    /// stripe sequentially (one lock at a time); called once per successful
    /// activation for the post-load batch notification.
    pub(crate) fn provided_names(&self, uid: u64) -> Vec<String> {
        let mut names = Vec::new();
        self.slots.for_each_shard(|slots| {
            for (name, bucket) in slots {
                if bucket.values().any(|provided| provided.provider_uid == uid) {
                    names.push(name.clone());
                }
            }
        });
        names
    }
}

/// Reverse indexes maintained by the kernel (D12), striped per map key (P1):
/// `key_interest` by service name, the edge maps by fiber uid. Edge updates
/// lock one stripe at a time and never hold two stripes at once; no consumer
/// relied on the pre-P1 cross-edge atomicity — the invariant layer tolerates
/// dispose-raced absence and `notify` re-resolves every candidate against
/// the live store.
#[derive(Debug, Default)]
pub struct DependencyIndex {
    /// Service name -> uids of fibers that declare an injection for it.
    key_interest: ShardMap<String, HashSet<u64>>,
    /// Provider uid -> uids of consumers whose committed views reference it.
    provider_consumers: ShardMap<u64, HashSet<u64>>,
    /// Consumer uid -> provider uids it currently depends on (cleanup mirror
    /// of `provider_consumers`).
    consumer_providers: ShardMap<u64, HashSet<u64>>,
}

impl DependencyIndex {
    /// Registers a fiber's declared interest in its injected service names.
    pub(crate) fn add_interest(&self, uid: u64, names: &[String]) {
        for name in names {
            self.key_interest.write(name, |interest| {
                interest.entry(name.clone()).or_default().insert(uid);
            });
        }
    }

    /// Appends the uids of fibers interested in `name` to `out`.
    pub(crate) fn interested(&self, name: &str, out: &mut Vec<u64>) {
        self.key_interest.read(name, |interest| {
            if let Some(set) = interest {
                out.extend(set.iter().copied());
            }
        });
    }

    /// True when `uid` is registered as interested in `name` (invariant
    /// checks).
    #[cfg(debug_assertions)]
    pub(crate) fn interest_has(&self, name: &str, uid: u64) -> bool {
        self.key_interest.read(name, |interest| {
            interest.is_some_and(|set| set.contains(&uid))
        })
    }

    /// True when any interest set still references `uid` (invariant checks).
    #[cfg(debug_assertions)]
    pub(crate) fn any_interest_has(&self, uid: u64) -> bool {
        let mut found = false;
        self.key_interest.for_each_shard(|interest| {
            found |= interest.values().any(|set| set.contains(&uid));
        });
        found
    }

    /// Drops all index entries for a disposed fiber.
    pub(crate) fn remove_fiber(&self, uid: u64, interested: &[String]) {
        for name in interested {
            self.key_interest.write(name, |interest| {
                if let Some(set) = interest.get_mut(name) {
                    set.remove(&uid);
                    if set.is_empty() {
                        interest.remove(name);
                    }
                }
            });
        }
        let providers = self
            .consumer_providers
            .write(&uid, |edges| edges.remove(&uid));
        if let Some(providers) = providers {
            self.drop_consumer_edges(uid, &providers);
        }
    }

    /// Records the dependency edges of a fiber's committed view.
    ///
    /// `providers` maps service name -> (label, provider uid). Each edge is
    /// written under one stripe lock at a time — two stripes are never held
    /// together, so concurrent commits for different fibers cannot deadlock.
    pub(crate) fn commit_edges(&self, uid: u64, providers: &HashMap<String, (u64, u64)>) {
        for &(_, provider) in providers.values() {
            self.provider_consumers.write(&provider, |consumers| {
                consumers.entry(provider).or_default().insert(uid);
            });
            self.consumer_providers.write(&uid, |edges| {
                edges.entry(uid).or_default().insert(provider);
            });
        }
    }

    /// Drops a fiber's committed edges before its store is cleared.
    pub(crate) fn uncommit(&self, uid: u64) {
        let providers = self
            .consumer_providers
            .write(&uid, |edges| edges.remove(&uid));
        if let Some(providers) = providers {
            self.drop_consumer_edges(uid, &providers);
        }
    }

    /// Removes `uid` from every listed provider's consumer set, dropping
    /// emptied sets.
    fn drop_consumer_edges(&self, uid: u64, providers: &HashSet<u64>) {
        for provider in providers {
            self.provider_consumers.write(provider, |consumers| {
                if let Some(set) = consumers.get_mut(provider) {
                    set.remove(&uid);
                    if set.is_empty() {
                        consumers.remove(provider);
                    }
                }
            });
        }
    }

    /// True when at least one committed consumer references `provider_uid`.
    #[must_use]
    pub fn relied(&self, provider_uid: u64) -> bool {
        self.provider_consumers.read(&provider_uid, |consumers| {
            consumers.is_some_and(|set| !set.is_empty())
        })
    }

    /// The consumer's provider set, cloned (invariant checks).
    #[cfg(debug_assertions)]
    pub(crate) fn consumer_providers_of(&self, uid: u64) -> Option<HashSet<u64>> {
        self.consumer_providers
            .read(&uid, |entry: Option<&HashSet<u64>>| entry.cloned())
    }

    /// True when `provider`'s consumer set references `uid` (invariant
    /// checks).
    #[cfg(debug_assertions)]
    pub(crate) fn provider_has_consumer(&self, provider: u64, uid: u64) -> bool {
        self.provider_consumers.read(&provider, |consumers| {
            consumers.is_some_and(|set| set.contains(&uid))
        })
    }

    /// True when `uid` exists as a provider key (invariant checks).
    #[cfg(debug_assertions)]
    pub(crate) fn has_provider_entry(&self, uid: u64) -> bool {
        self.provider_consumers.read(&uid, |entry| entry.is_some())
    }

    /// True when any provider's consumer set references `uid` (invariant
    /// checks).
    #[cfg(debug_assertions)]
    pub(crate) fn any_provider_references(&self, uid: u64) -> bool {
        let mut found = false;
        self.provider_consumers.for_each_shard(|consumers| {
            found |= consumers.values().any(|set| set.contains(&uid));
        });
        found
    }

    /// Adds a forward edge without its reverse edge (invariant-layer fault
    /// injection).
    #[cfg(test)]
    pub(crate) fn test_add_consumer_provider(&self, uid: u64, provider: u64) {
        self.consumer_providers.write(&uid, |edges| {
            edges.entry(uid).or_default().insert(provider);
        });
    }

    /// Removes a reverse edge without touching its forward edge
    /// (invariant-layer fault injection).
    #[cfg(test)]
    pub(crate) fn test_remove_provider_consumer(&self, provider: u64, uid: u64) {
        self.provider_consumers.write(&provider, |consumers| {
            if let Some(set) = consumers.get_mut(&provider) {
                set.remove(&uid);
            }
        });
    }
}

/// Resolves the implementation snapshot a fiber would commit for `name`.
///
/// Labels are tried in D15 precedence order (explicit per-name isolation,
/// then the innermost realm, then the global default); the first label with
/// a live implementation wins. Returns `None` when no candidate resolves,
/// the provider is inactive (strict), or the availability predicate fails.
pub fn resolve_impl_for(ctx: &Context, dependent: &Fiber, name: &str) -> Option<ProviderSnapshot> {
    let root = &ctx.root;
    for label in ScopeNode::label_candidates(Some(dependent.scope_node()), name).as_slice() {
        let provided = root.reflect.get_impl(root, name, *label, true);
        if let Some(provided) = provided {
            if provided.compute.is_some() {
                // Accessor entries commit only liveness metadata; values are
                // computed per read against the reading context.
                return Some(ProviderSnapshot::dynamic(provided.provider_uid));
            }
            return Some(ProviderSnapshot {
                provider_uid: provided.provider_uid,
                value: Arc::clone(&provided.value),
                dynamic: false,
            });
        }
    }
    None
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
    let label = ScopeNode::primary_label(Some(&ctx.scope), name);
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
        .effect(
            format!("ctx.provide({name:?})"),
            // Service withdrawal is kernel-structural: it must finish inside
            // the unload path (S3); its inner consumer wait is bounded by the
            // drain budget below.
            crate::fiber::Teardown::Critical,
            async move {
                // S6 (D21): the slot mutation, self-commit, and dependent
                // refresh form one serial section per service group, so a
                // concurrent update to the same slot can never slip between
                // them (which used to be able to lose a withdrawal event).
                let key = crate::ordering::GroupKey::new(label, &name_owned);
                let section_ctx = effect_ctx.clone();
                let section_name = name_owned.clone();
                effect_ctx.root.ordering.run(&key, move || -> Result<()> {
                    section_ctx.root.reflect.insert(provided, label, &owner)?;
                    // The provider must see itself during activation (TS
                    // writes the impl into its own committed view).
                    fiber_in.commit_self(
                        &section_name,
                        crate::fiber::ProviderSnapshot {
                            provider_uid: uid,
                            value: self_value,
                            dynamic: false,
                        },
                    );
                    if fiber_in.state() == FiberState::Active {
                        notify(&section_ctx, std::slice::from_ref(&section_name));
                    }
                    Ok(())
                })?;
                let disposer_name = name_owned.clone();
                let disposer_ctx = effect_ctx.clone();
                let disposer: crate::fiber::Disposer = Box::new(move || {
                    let name = disposer_name.clone();
                    let ctx = disposer_ctx.clone();
                    Box::pin(async move {
                        // Withdrawal protocol: wake dependents, then wait for
                        // them to converge before returning. Consumer join
                        // errors are the consumer's own startup failures, already
                        // logged there; the drain only needs convergence. With a
                        // drain budget (S3) the per-consumer wait is bounded and
                        // the configured policy runs on a timeout.
                        //
                        // S6 (D21): removal + notification are one serial
                        // section; the drain waits *outside* it so a stuck
                        // consumer cannot block later updates of this group.
                        let touched = ctx.root.ordering.run(&key, || {
                            ctx.root.reflect.remove(&name, label);
                            notify(&ctx, &[name])
                        });
                        drain_consumers(&ctx, touched).await;
                    })
                });
                Ok(Some(disposer))
            },
        )
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
    let label = ScopeNode::primary_label(Some(&ctx.scope), name);
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
        .effect(
            format!("ctx.accessor({name:?})"),
            crate::fiber::Teardown::Critical,
            async move {
                // S6 (D21): same serial-section discipline as `provide`.
                let key = crate::ordering::GroupKey::new(label, &name_owned);
                let section_ctx = effect_ctx.clone();
                let section_name = name_owned.clone();
                effect_ctx.root.ordering.run(&key, move || -> Result<()> {
                    section_ctx.root.reflect.insert(provided, label, &owner)?;
                    fiber_in
                        .commit_self(&section_name, crate::fiber::ProviderSnapshot::dynamic(uid));
                    if fiber_in.state() == FiberState::Active {
                        notify(&section_ctx, std::slice::from_ref(&section_name));
                    }
                    Ok(())
                })?;
                let disposer_ctx = effect_ctx.clone();
                let disposer_name = name_owned.clone();
                let disposer: crate::fiber::Disposer = Box::new(move || {
                    let name = disposer_name.clone();
                    let ctx = disposer_ctx.clone();
                    Box::pin(async move {
                        let touched = ctx.root.ordering.run(&key, || {
                            ctx.root.reflect.remove(&name, label);
                            notify(&ctx, &[name])
                        });
                        drain_consumers(&ctx, touched).await;
                    })
                });
                Ok(Some(disposer))
            },
        )
        .await
}

/// Overwrites the value of a service provided by the current fiber.
///
/// # Errors
///
/// Fails when the service is not provided by this fiber.
pub fn set(ctx: &Context, name: &str, value: Arc<dyn Any + Send + Sync>) -> Result<()> {
    let fiber = ctx.fiber().ok_or_else(Error::inactive_effect)?;
    let label = ScopeNode::primary_label(Some(&ctx.scope), name);
    let uid = fiber.uid_value().ok_or_else(Error::inactive_effect)?;
    ctx.root.reflect.overwrite(name, label, uid, value)
}

/// Awaits every touched consumer's convergence inside a provider withdrawal,
/// under the drain budget (S3).
///
/// Without a budget the wait is unbounded (the pre-S3 behavior, paper Thm
/// 63). With one, a consumer that misses its budget is logged and handled per
/// policy: [`crate::timeout::DrainPolicy::LogAndContinue`] moves on to the
/// next consumer; [`crate::timeout::DrainPolicy::CancelConsumer`] additionally
/// cancels the stuck consumer's task scope, which lands an abort-safe hang
/// `Failed` (a tracked foreign call is unaffected — the reap watchdog
/// surfaces it).
async fn drain_consumers(ctx: &Context, touched: Vec<Arc<Fiber>>) {
    let drain = ctx.root.timeouts.drain;
    for fiber in touched {
        let Some(drain) = drain else {
            let _ = fiber.join().await;
            continue;
        };
        if tokio::time::timeout(drain.budget, fiber.join())
            .await
            .is_ok()
        {
            continue;
        }
        ctx.root.policies.emit(
            &*ctx.logger(),
            crate::ErrorClass::Timeout,
            LogRecord::new(
                LogLevel::Error,
                "service",
                "drain_timeout",
                format!("consumer missed the {:?} drain budget", drain.budget),
            )
            .with_fiber(fiber.name(), fiber.uid_value())
            .with_duration(drain.budget),
        );
        if drain.policy == crate::timeout::DrainPolicy::CancelConsumer {
            fiber.current_scope().cancel();
        }
    }
}

/// Re-evaluates every interested fiber for the given service names and emits
/// `internal/service` once per name.
///
/// Returns the fibers whose dependency state changed (the withdrawal protocol
/// awaits them).
///
/// Lock discipline (P1): candidate uids are collected from the interest
/// index and upgraded to strong fiber references one stripe lock at a time;
/// no root-state lock is then held while a fiber re-resolves (`check_impl`
/// touches the store and the liveness set). The pre-P1 code held the global
/// fiber-table lock across every `check_impl`, which both serialized
/// unrelated notifies and — for services with an availability predicate —
/// deadlocked: the strict check re-locked the fiber table from the same
/// thread. A fiber disposed between upgrade and re-resolution is harmless:
/// its `refresh` is refused by the disposed choke-point guard (S4).
pub fn notify(ctx: &Context, names: &[String]) -> Vec<Arc<Fiber>> {
    let mut candidates: Vec<u64> = Vec::new();
    for name in names {
        ctx.root.index.interested(name, &mut candidates);
    }
    candidates.sort_unstable();
    candidates.dedup();
    let mut touched = Vec::new();
    for uid in candidates {
        let Some(fiber) = ctx.root.fiber_by_uid(uid) else {
            continue;
        };
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
    for name in names {
        let value = ctx
            .root
            .reflect
            .get_impl(&ctx.root, name, ctx.effective_label(name), false)
            .map(|p| p.value);
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
