// Rust guideline compliant 2026-10-24
//! State-machine invariant checkpoints (improvement track 二.1).
//!
//! Every fiber lifecycle transition passes the checkpoint here that
//! re-validates the uid/store/index/epoch consistency the kernel relies on.
//! A breach is a kernel programming error, so checkpoints panic with a
//! locating snapshot instead of returning errors nobody could act on.
//!
//! Two tiers share every checkpoint:
//!
//! - release tier: O(1) field relations (epoch equalities, uid/error
//!   presence, cancel-scope draining), always compiled and always on;
//! - debug tier: cross-structure consistency (committed view vs dependency
//!   index, store completeness, fiber-table/active-set/interest membership),
//!   compiled only under `debug_assertions`.
//!
//! Checkpoint conditions are formulated to stay race-free against a
//! concurrent [`Fiber::dispose`], which legitimately rewrites the uid, the
//! fiber table, the active set, and the dependency index from a non-driver
//! task. Deep checks therefore collect the first violation while holding a
//! lock and breach only after dropping it (a breach itself locks `loaded`
//! and `driver` for the snapshot, and `parking_lot` mutexes are not
//! reentrant). Shared-structure entries that are *present* must match
//! exactly, while *absent* entries are tolerated (a racing dispose removes
//! them). Fields with a single writer (`loaded`, `store`: the driver task
//! alone) are checked unconditionally.

use std::collections::HashMap;

use crate::fiber::{EPOCH_INACTIVE, Fiber, ProviderSnapshot};

/// Asserts one invariant condition, breaching with a fiber snapshot.
macro_rules! invariant {
    ($fiber:expr, $checkpoint:expr, $cond:expr, $($what:tt)*) => {
        if !$cond {
            $crate::invariant::breach($fiber, $checkpoint, &format!($($what)*));
        }
    };
}

/// Terminates on an invariant breach with a locating fiber snapshot.
///
/// A breach is a kernel programming error, so this panics rather than
/// returning an error no caller could act on.
pub fn breach(fiber: &Fiber, checkpoint: &str, what: &str) -> ! {
    let (desired, dirty) = fiber.driver_snapshot();
    let snapshot = format!(
        "dougong invariant breach at {checkpoint}: {what} \
         (fiber={:?}, uid={:?}, state={}, loaded={:?}, desired={desired:?}, dirty={dirty})",
        fiber.name(),
        fiber.uid_value(),
        fiber.state().as_str(),
        fiber.epoch(),
    );
    // Panic is deliberate: an invariant breach is a kernel programming error
    // (M-PANIC-ON-BUG), not a runtime condition callers could handle.
    panic!("{snapshot}"); // justified: contract violation on kernel invariant
}

/// Validates a freshly registered fiber, before the cascade effect exists.
///
/// Runs inside [`crate::registry::plugin`] before the parent cascade effect
/// is registered, so no dispose can race the membership checks.
///
/// # Panics
///
/// Panics when the fiber has no uid or (debug builds) is missing from the
/// fiber table or the interest index.
pub fn post_spawn(fiber: &Fiber) {
    let uid = fiber.uid_value();
    invariant!(
        fiber,
        "post_spawn",
        uid.is_some(),
        "fresh fiber must own a uid"
    );
    #[cfg(debug_assertions)]
    post_spawn_deep(fiber, uid);
}

/// Debug tier of [`post_spawn`]: fiber-table and interest membership.
#[cfg(debug_assertions)]
fn post_spawn_deep(fiber: &Fiber, uid: Option<u64>) {
    let Some(uid) = uid else {
        return; // the release tier already breached on the missing uid
    };
    let root = fiber.ctx().root;
    let registered = root.fibers.read(&uid, |slot| {
        slot.is_some_and(|weak| weak.upgrade().is_some())
    });
    let violation = (!registered).then(|| format!("fiber table missing fresh uid {uid}"));
    if let Some(what) = violation {
        breach(fiber, "post_spawn", &what);
    }
    let violation = fiber.inject.iter().find_map(|injection| {
        let name = injection.service.as_ref();
        if root.index.interest_has(name, uid) {
            None
        } else {
            Some(format!("key_interest[{name:?}] missing fresh uid {uid}"))
        }
    });
    if let Some(what) = violation {
        breach(fiber, "post_spawn", &what);
    }
}

/// Validates the `load_once` entry condition.
///
/// The uid is deliberately not asserted: a concurrent dispose may clear it
/// between the driver's stale-target pre-check and this checkpoint.
///
/// # Panics
///
/// Panics when the target is the inactive epoch or the fiber is still loaded.
pub fn pre_load(fiber: &Fiber, target: &str) {
    invariant!(
        fiber,
        "pre_load",
        target != EPOCH_INACTIVE,
        "load target must be activatable, got {target:?}"
    );
    invariant!(
        fiber,
        "pre_load",
        fiber.loaded_eq(EPOCH_INACTIVE),
        "load requires an unloaded fiber, loaded={:?}",
        fiber.epoch()
    );
}

/// Validates the success arm of `load_once`.
///
/// `committed` is the candidate view committed as ω before the body ran.
///
/// # Panics
///
/// Panics when the loaded epoch diverges from the target or (debug builds)
/// the store, index, or state consistency is breached.
pub fn post_load(fiber: &Fiber, target: &str, committed: &HashMap<String, ProviderSnapshot>) {
    invariant!(
        fiber,
        "post_load",
        fiber.loaded_eq(target),
        "loaded epoch must equal the load target {target:?}"
    );
    let _ = committed;
    #[cfg(debug_assertions)]
    post_load_deep(fiber, target, committed);
}

/// Debug tier of [`post_load`]: store completeness and index consistency.
#[cfg(debug_assertions)]
fn post_load_deep(fiber: &Fiber, target: &str, committed: &HashMap<String, ProviderSnapshot>) {
    let self_uid = fiber.uid_value();
    // Every committed dependency must be readable in the store, either with
    // the committed provider or overwritten by the fiber's own `commit_self`.
    let violation = {
        let store = fiber.store.lock();
        committed
            .iter()
            .find_map(|(name, snapshot)| match store.get(name) {
                None => Some(format!("store missing committed dependency {name:?}")),
                Some(live)
                    if live.provider_uid != snapshot.provider_uid
                        && Some(live.provider_uid) != self_uid =>
                {
                    Some(format!(
                        "store entry {name:?} points at provider {}, expected {} or self",
                        live.provider_uid, snapshot.provider_uid
                    ))
                }
                Some(_) => None,
            })
    };
    if let Some(what) = violation {
        breach(fiber, "post_load", &what);
    }
    // Index consistency, tolerant form: a concurrently disposed fiber already
    // had its edges removed, so absence is tolerated; present edges must
    // equal the committed provider set with reverse edges intact. The index
    // is striped per key (P1), so the forward set is cloned first and the
    // reverse edges are probed one stripe at a time; a missing reverse edge
    // is only a breach when the fiber still owns its uid — a racing dispose
    // (which clears the uid *before* removing edges) is the sole legitimate
    // remover, since the fiber's own commit/uncommit run on this driver.
    if let Some(uid) = self_uid {
        let violation = {
            let root = fiber.ctx().root;
            let index = &root.index;
            index.consumer_providers_of(uid).and_then(|providers| {
                let expected: std::collections::HashSet<u64> = committed
                    .values()
                    .map(|snapshot| snapshot.provider_uid)
                    .collect();
                if providers != expected {
                    return Some(format!(
                        "consumer_providers[{uid}] = {providers:?}, expected {expected:?}"
                    ));
                }
                providers.iter().find_map(|provider| {
                    if index.provider_has_consumer(*provider, uid) || fiber.uid_value().is_none() {
                        None
                    } else {
                        Some(format!(
                            "reverse edge provider_consumers[{provider}] missing consumer {uid}"
                        ))
                    }
                })
            })
        };
        if let Some(what) = violation {
            breach(fiber, "post_load", &what);
        }
    }
    // State mirror: a driver quiesced on this target must read Active.
    // Skipped under a racing re-arm (dirty) or a racing dispose (uid gone).
    let (desired, dirty) = fiber.driver_snapshot();
    if !dirty && desired == target && fiber.uid_value().is_some() {
        invariant!(
            fiber,
            "post_load",
            fiber.state() == crate::fiber::FiberState::Active,
            "quiesced on {target:?} but state is {}",
            fiber.state().as_str()
        );
    }
}

/// Validates the `unload_once` entry condition.
///
/// # Panics
///
/// Panics when the fiber carries no loaded epoch to unload.
pub fn pre_unload(fiber: &Fiber) {
    invariant!(
        fiber,
        "pre_unload",
        !fiber.loaded_eq(EPOCH_INACTIVE),
        "unload requires a loaded fiber"
    );
}

/// Validates the end of `unload_once`.
///
/// # Panics
///
/// Panics when the loaded epoch survived or (debug builds) store or index
/// residue remains.
pub fn post_unload(fiber: &Fiber) {
    invariant!(
        fiber,
        "post_unload",
        fiber.loaded_eq(EPOCH_INACTIVE),
        "unload must reset the loaded epoch"
    );
    // S2: a cancelled task scope must have drained by the time the unload
    // lands — no listener invocation outlives the teardown effects.
    invariant!(
        fiber,
        "post_unload",
        fiber.scope_reaped(),
        "cancelled task scope must drain to zero live tasks"
    );
    #[cfg(debug_assertions)]
    check_cleared(fiber, "post_unload");
}

/// Validates the parked state after `fail_load`.
///
/// The desired epoch must be inactive unless a concurrent notification
/// already re-armed convergence (`dirty`).
///
/// # Panics
///
/// Panics when the fiber is still loaded, the error was not recorded, the
/// driver is not parked, or (debug builds) store or index residue remains.
pub fn post_fail_load(fiber: &Fiber) {
    invariant!(
        fiber,
        "post_fail_load",
        fiber.loaded_eq(EPOCH_INACTIVE),
        "failure must reset the loaded epoch"
    );
    invariant!(
        fiber,
        "post_fail_load",
        fiber.has_error(),
        "failure must record the startup error"
    );
    let (desired, dirty) = fiber.driver_snapshot();
    invariant!(
        fiber,
        "post_fail_load",
        desired == EPOCH_INACTIVE || dirty,
        "failed fiber parks at the inactive epoch unless re-armed, \
         desired={desired:?} dirty={dirty}"
    );
    // S2: the rollback path reaps the scope before parking.
    invariant!(
        fiber,
        "post_fail_load",
        fiber.scope_reaped(),
        "cancelled task scope must drain to zero live tasks"
    );
    #[cfg(debug_assertions)]
    check_cleared(fiber, "post_fail_load");
}

/// Debug tier shared by [`post_unload`] and [`post_fail_load`].
///
/// A dispose clears `uid` before the driver unloads; commit and uncommit
/// then both key on the sentinel uid 0 (symmetric), so a missing uid falls
/// back to the sentinel-side residue check.
///
/// The own-provider-arm residue check (`provider_consumers[uid]` keyed by
/// the unloading fiber) rests on the drain protocol waiting for every
/// consumer to uncommit; a configured drain budget (S3) weakens that wait,
/// so a stuck consumer's stale edge may outlive the withdrawal — it
/// self-cleans when the consumer later converges or is disposed.
#[cfg(debug_assertions)]
fn check_cleared(fiber: &Fiber, checkpoint: &'static str) {
    let violation = (!fiber.store.lock().is_empty()).then(|| "store must be cleared".to_string());
    if let Some(what) = violation {
        breach(fiber, checkpoint, &what);
    }
    let violation = {
        let root = fiber.ctx().root;
        let index = &root.index;
        let drain_bounded = root.timeouts.drain.is_some();
        fiber.uid_value().map_or_else(
            || sentinel_residue(index),
            |uid| consumer_residue(index, uid, drain_bounded),
        )
    };
    if let Some(what) = violation {
        breach(fiber, checkpoint, &what);
    }
}

/// Index residue check for a live uid at unload/fail checkpoints.
///
/// Runs only while the fiber still owns its uid, so a racing `remove_fiber`
/// (which always follows the uid claim) cannot be mid-cleanup here; the
/// fiber's own `uncommit` already ran on this driver.
#[cfg(debug_assertions)]
fn consumer_residue(
    index: &crate::service::DependencyIndex,
    uid: u64,
    drain_bounded: bool,
) -> Option<String> {
    if index.consumer_providers_of(uid).is_some() {
        Some(format!("consumer_providers still holds {uid}"))
    } else if !drain_bounded && index.has_provider_entry(uid) {
        Some(format!("provider_consumers still lists {uid} as provider"))
    } else if index.any_provider_references(uid) {
        Some(format!(
            "provider_consumers still references consumer {uid}"
        ))
    } else {
        None
    }
}

/// Index residue check for the sentinel uid 0 (dispose cleared `uid` before
/// the driver unloaded). Root-provided builtins make `provider_consumers[0]`
/// legitimate, so only the consumer direction is checked.
#[cfg(debug_assertions)]
fn sentinel_residue(index: &crate::service::DependencyIndex) -> Option<String> {
    if index.consumer_providers_of(0).is_some() {
        Some("consumer_providers still holds sentinel uid 0".to_string())
    } else if index.any_provider_references(0) {
        Some("provider_consumers still references sentinel uid 0".to_string())
    } else {
        None
    }
}

/// Validates the end of [`Fiber::dispose`].
///
/// # Panics
///
/// Panics when the uid survived, the fiber is still loaded, or (debug
/// builds) the fiber table, active set, or indexes still reference the uid.
pub fn post_dispose(fiber: &Fiber, old_uid: u64) {
    invariant!(
        fiber,
        "post_dispose",
        fiber.uid_value().is_none(),
        "dispose must clear the uid"
    );
    invariant!(
        fiber,
        "post_dispose",
        fiber.loaded_eq(EPOCH_INACTIVE),
        "dispose must settle unloaded"
    );
    // S2: dispose cancelled the scope and the driver reaped it before landing.
    invariant!(
        fiber,
        "post_dispose",
        fiber.scope_reaped(),
        "cancelled task scope must drain to zero live tasks"
    );
    let _ = old_uid; // consumed by the debug tier only
    #[cfg(debug_assertions)]
    post_dispose_deep(fiber, old_uid);
}

/// Debug tier of [`post_dispose`]; the root fiber (uid 0) is exempt because
/// it is never deregistered.
#[cfg(debug_assertions)]
fn post_dispose_deep(fiber: &Fiber, old_uid: u64) {
    if old_uid == 0 {
        return;
    }
    let root = fiber.ctx().root;
    let violation = root
        .fibers
        .read(&old_uid, |slot| slot.is_some())
        .then(|| format!("fiber table still holds {old_uid}"));
    if let Some(what) = violation {
        breach(fiber, "post_dispose", &what);
    }
    let violation = root
        .active
        .contains(old_uid)
        .then(|| format!("active set still holds {old_uid}"));
    if let Some(what) = violation {
        breach(fiber, "post_dispose", &what);
    }
    let violation = {
        let index = &root.index;
        // The own-provider arm rests on the drain protocol's unbounded wait;
        // a configured drain budget (S3) lets a stuck consumer's stale edge
        // outlive the withdrawal (self-cleaning on its later convergence or
        // disposal). The dispose winner runs `remove_fiber` before its join,
        // so by this checkpoint every cleanup has already landed.
        let drain_bounded = root.timeouts.drain.is_some();
        if index.consumer_providers_of(old_uid).is_some() {
            Some(format!("consumer_providers still holds {old_uid}"))
        } else if !drain_bounded && index.has_provider_entry(old_uid) {
            Some(format!(
                "provider_consumers still lists {old_uid} as provider"
            ))
        } else if index.any_provider_references(old_uid) {
            Some(format!(
                "provider_consumers still references consumer {old_uid}"
            ))
        } else if index.any_interest_has(old_uid) {
            Some(format!("key_interest still references {old_uid}"))
        } else {
            None
        }
    };
    if let Some(what) = violation {
        breach(fiber, "post_dispose", &what);
    }
}

/// Validates the driver's landing condition: settled means loaded == desired.
///
/// Both epochs are captured by the caller while holding the driver lock, so
/// the comparison reflects one atomic landing snapshot; a breach fires after
/// that lock was dropped.
///
/// # Panics
///
/// Panics when the committed epoch diverges from the desired epoch.
pub fn settled(fiber: &Fiber, loaded: &str, desired: &str) {
    invariant!(
        fiber,
        "settled",
        loaded == desired,
        "driver settled with loaded={loaded:?} desired={desired:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use crate::fiber::FiberHandle;
    use crate::plugin::{Injection, plugin};
    use std::sync::Arc;

    /// Boots a settled provider/consumer pair; returns the consumer handle.
    async fn boot_consumer() -> (Context, FiberHandle) {
        let ctx = Context::new();
        let provider = plugin("provider", vec![], |ctx, _config| {
            Box::pin(async move {
                ctx.provide("svc", 42_i32).await?;
                Ok(())
            })
        });
        let consumer = plugin("consumer", vec![Injection::new("svc")], |_ctx, _config| {
            Box::pin(async move { Ok(()) })
        });
        ctx.plugin(provider, None)
            .expect("spawn provider")
            .join()
            .await
            .expect("provider active");
        let consumer = ctx.plugin(consumer, None).expect("spawn consumer");
        consumer.join().await.expect("consumer active");
        (ctx, consumer)
    }

    /// Spawns a fiber that stays Pending (its dependency never resolves).
    fn spawn_pending(ctx: &Context) -> FiberHandle {
        let pending = plugin(
            "pending",
            vec![Injection::new("missing")],
            |_ctx, _config| Box::pin(async move { Ok(()) }),
        );
        ctx.plugin(pending, None).expect("spawn pending")
    }

    /// Spawns a fiber whose body fails; returns the parked Failed handle.
    async fn spawn_failed(ctx: &Context) -> FiberHandle {
        let boom = plugin("boom", vec![], |_ctx, _config| {
            Box::pin(async move { Err(crate::error::Error::msg("boom")) })
        });
        let handle = ctx.plugin(boom, None).expect("spawn failing");
        handle.join().await.expect_err("startup must fail");
        handle
    }

    #[tokio::test]
    async fn healthy_world_passes_every_checkpoint() {
        let (_ctx, consumer) = boot_consumer().await;
        let fiber = &consumer.0;
        post_spawn(fiber);
        pre_unload(fiber);
        let committed = fiber.candidates.lock().clone();
        let epoch = fiber.epoch();
        post_load(fiber, &epoch, &committed);
        settled(fiber, &epoch, &epoch);
        let uid = fiber.uid_value().expect("uid");
        consumer.dispose().await;
        // The internal post_dispose passed; an idempotent re-check holds too.
        post_dispose(fiber, uid);
    }

    #[tokio::test]
    async fn healthy_pending_fiber_passes_unload_and_spawn_checks() {
        let ctx = Context::new();
        let handle = spawn_pending(&ctx);
        post_spawn(&handle.0);
        pre_load(&handle.0, ":1");
        post_unload(&handle.0);
    }

    #[tokio::test]
    #[should_panic(expected = "post_load: store missing committed dependency")]
    async fn post_load_detects_missing_store_entry() {
        let (_ctx, consumer) = boot_consumer().await;
        let fiber = &consumer.0;
        fiber.store.lock().remove("svc");
        let committed = fiber.candidates.lock().clone();
        let epoch = fiber.epoch();
        post_load(fiber, &epoch, &committed);
    }

    #[tokio::test]
    #[should_panic(expected = "post_load: store entry")]
    async fn post_load_detects_wrong_provider() {
        let (_ctx, consumer) = boot_consumer().await;
        let fiber = &consumer.0;
        fiber
            .store
            .lock()
            .get_mut("svc")
            .expect("store entry")
            .provider_uid = u64::MAX;
        let committed = fiber.candidates.lock().clone();
        let epoch = fiber.epoch();
        post_load(fiber, &epoch, &committed);
    }

    #[tokio::test]
    #[should_panic(expected = "post_load: consumer_providers")]
    async fn post_load_detects_extra_index_edge() {
        let (ctx, consumer) = boot_consumer().await;
        let fiber = &consumer.0;
        let uid = fiber.uid_value().expect("uid");
        ctx.root.index.test_add_consumer_provider(uid, u64::MAX);
        let committed = fiber.candidates.lock().clone();
        let epoch = fiber.epoch();
        post_load(fiber, &epoch, &committed);
    }

    #[tokio::test]
    #[should_panic(expected = "post_load: reverse edge")]
    async fn post_load_detects_missing_reverse_edge() {
        let (ctx, consumer) = boot_consumer().await;
        let fiber = &consumer.0;
        let uid = fiber.uid_value().expect("uid");
        let provider_uid = fiber
            .candidates
            .lock()
            .get("svc")
            .expect("candidate")
            .provider_uid;
        ctx.root
            .index
            .test_remove_provider_consumer(provider_uid, uid);
        let committed = fiber.candidates.lock().clone();
        let epoch = fiber.epoch();
        post_load(fiber, &epoch, &committed);
    }

    #[tokio::test]
    async fn post_load_tolerates_fully_cleaned_edges() {
        // A racing dispose removes the consumer's edges; absence is tolerated.
        let (ctx, consumer) = boot_consumer().await;
        let fiber = &consumer.0;
        let uid = fiber.uid_value().expect("uid");
        ctx.root.index.uncommit(uid);
        let committed = fiber.candidates.lock().clone();
        let epoch = fiber.epoch();
        post_load(fiber, &epoch, &committed);
    }

    #[tokio::test]
    #[should_panic(expected = "pre_load: load requires an unloaded fiber")]
    async fn pre_load_rejects_loaded_fiber() {
        let (_ctx, consumer) = boot_consumer().await;
        pre_load(&consumer.0, ":1");
    }

    #[tokio::test]
    #[should_panic(expected = "post_unload: unload must reset the loaded epoch")]
    async fn post_unload_rejects_loaded_fiber() {
        let (_ctx, consumer) = boot_consumer().await;
        post_unload(&consumer.0);
    }

    #[tokio::test]
    #[should_panic(expected = "post_unload: cancelled task scope must drain")]
    async fn post_unload_detects_unreaped_task_scope() {
        let ctx = Context::new();
        let handle = spawn_pending(&ctx);
        let scope = handle.0.current_scope();
        let _live = scope
            .spawn(std::future::pending::<()>())
            .expect("scope admits the task");
        // Cancel without reaping. On the current-thread test runtime the
        // spawned task is never polled before the (synchronous) checkpoint
        // runs, so the live-task count is deterministically one here.
        scope.cancel();
        post_unload(&handle.0);
    }

    #[tokio::test]
    #[should_panic(expected = "post_unload: consumer_providers still holds")]
    async fn post_unload_detects_stale_edge() {
        let ctx = Context::new();
        let handle = spawn_pending(&ctx);
        let uid = handle.0.uid_value().expect("uid");
        ctx.root.index.test_add_consumer_provider(uid, u64::MAX);
        post_unload(&handle.0);
    }

    #[tokio::test]
    async fn failed_fiber_passes_post_fail_load_and_tolerates_rearm() {
        let ctx = Context::new();
        let handle = spawn_failed(&ctx).await;
        post_fail_load(&handle.0);
        // A notification that re-arms convergence mid-fail is legal.
        handle.0.test_set_driver(":1".to_string(), true);
        post_fail_load(&handle.0);
    }

    #[tokio::test]
    #[should_panic(expected = "post_fail_load: failed fiber parks")]
    async fn post_fail_load_rejects_unarmed_desired() {
        let ctx = Context::new();
        let handle = spawn_failed(&ctx).await;
        handle.0.test_set_driver(":1".to_string(), false);
        post_fail_load(&handle.0);
    }

    #[tokio::test]
    #[should_panic(expected = "post_fail_load: failure must record")]
    async fn post_fail_load_requires_recorded_error() {
        let ctx = Context::new();
        let handle = spawn_pending(&ctx);
        post_fail_load(&handle.0);
    }

    #[tokio::test]
    #[should_panic(expected = "post_dispose: dispose must clear the uid")]
    async fn post_dispose_rejects_live_fiber() {
        let (_ctx, consumer) = boot_consumer().await;
        let uid = consumer.0.uid_value().expect("uid");
        post_dispose(&consumer.0, uid);
    }

    #[tokio::test]
    #[should_panic(expected = "post_dispose: fiber table still holds")]
    async fn post_dispose_detects_leaked_registration() {
        let (ctx, consumer) = boot_consumer().await;
        let uid = consumer.0.uid_value().expect("uid");
        consumer.dispose().await;
        // Corrupt: re-register the dead fiber in the table.
        ctx.root.fibers.write(&uid, |table| {
            table.insert(uid, Arc::downgrade(&consumer.0));
        });
        post_dispose(&consumer.0, uid);
    }

    #[tokio::test]
    #[should_panic(expected = "settled: driver settled with")]
    async fn settled_detects_divergence() {
        let (_ctx, consumer) = boot_consumer().await;
        settled(&consumer.0, ":1", ":2");
    }
}
