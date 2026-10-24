// Rust guideline compliant 2026-10-25
//! Notify/coalescing benchmarks for the performance track P3 (improve
//! requirement 一.3): same-fiber refresh churn under concurrent replacement
//! storms, multi-service provider withdrawal fan-out, and transitive chain
//! refreshes.
//!
//! Ignored by default so the regular gates stay hermetic; run on demand:
//!
//! ```sh
//! cargo test --release -p dougong --test bench_notify -- --ignored --nocapture
//! ```
//!
//! Every scenario prints one machine-readable `BENCH name=... ops=...
//! wall_ms=... status_events=... consumer_status_events=...
//! service_events=...` line through the kernel logger (the repo's only
//! sanctioned output channel). `status_events` counts `internal/status`
//! republications — the "state-switch count" the requirement targets — and
//! `service_events` counts `internal/service` emissions, which the change
//! must not alter (the S6 event contract). Counters are anchored: final
//! states must converge and every counter must be positive, so a broken
//! kernel cannot produce "fast" numbers. Compare against the recorded
//! pre/post-change baselines on the same machine (findings §19).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use dougong::{Context, FiberHandle, FiberState, Injection, Listener, LogLevel, Plugin, plugin};
use serde_json::Value;

/// Concurrently replaced service slots in the replacement storm (B1).
const CHURN_SERVICES: usize = 4;
/// The churn slot names, indexed in replacement-task order.
const CHURN_SLOTS: [&str; CHURN_SERVICES] = ["b1.s0", "b1.s1", "b1.s2", "b1.s3"];
/// Consumers declaring every churn slot (B1) or every group service (B2).
const CONSUMERS: usize = 8;
/// Replacement rounds per churn task (B1) / provider lifecycle (B2, B3).
const ROUNDS: usize = 30;
/// Services provided by the single multi-service provider (B2).
const GROUP_SERVICES: usize = 4;
/// Hard ceiling for one scenario: a hang fails the bench instead of
/// stalling a manual run.
const RAIL: Duration = Duration::from_secs(180);

/// Shared event counters snapshotted around each measured window.
#[derive(Clone, Default)]
struct Counters {
    status: Arc<AtomicU64>,
    consumer_status: Arc<AtomicU64>,
    service: Arc<AtomicU64>,
}

impl Counters {
    fn new() -> Self {
        Self::default()
    }

    /// Registers the counting listeners on the root context. They ride the
    /// P2 queue like any fire-and-forget listener; deltas are read after a
    /// settle delay so delivery lag is bounded on both baseline runs.
    async fn install(self, ctx: &Context, consumer_prefix: &'static str) -> Self {
        let status_listener: Listener = {
            let status = Arc::clone(&self.status);
            let consumer_status = Arc::clone(&self.consumer_status);
            Arc::new(move |_ctx, payload, _next| {
                let status = Arc::clone(&status);
                let consumer_status = Arc::clone(&consumer_status);
                Box::pin(async move {
                    status.fetch_add(1, AtomicOrdering::Relaxed);
                    if payload
                        .get("fiber")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.starts_with(consumer_prefix))
                    {
                        consumer_status.fetch_add(1, AtomicOrdering::Relaxed);
                    }
                    Ok(Value::Null)
                })
            })
        };
        let service_listener: Listener = {
            let service = Arc::clone(&self.service);
            Arc::new(move |_ctx, _payload, _next| {
                let service = Arc::clone(&service);
                Box::pin(async move {
                    service.fetch_add(1, AtomicOrdering::Relaxed);
                    Ok(Value::Null)
                })
            })
        };
        ctx.on("internal/status", status_listener)
            .await
            .expect("register status counter");
        ctx.on("internal/service", service_listener)
            .await
            .expect("register service counter");
        self
    }

    fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.status.load(AtomicOrdering::Relaxed),
            self.consumer_status.load(AtomicOrdering::Relaxed),
            self.service.load(AtomicOrdering::Relaxed),
        )
    }
}

/// A plugin providing `service` with a constant value (1).
fn provider_plugin(name: &'static str, service: &'static str) -> Arc<dyn Plugin> {
    plugin(name, vec![], move |ctx, _config| {
        Box::pin(async move {
            ctx.provide(service, 1_u64).await?;
            Ok(())
        })
    })
}

/// A plugin providing every service in `services` (the multi-service
/// provider whose withdrawal fans out per service).
fn group_provider_plugin(name: &'static str, services: Vec<String>) -> Arc<dyn Plugin> {
    plugin(name, vec![], move |ctx, _config| {
        let services = services.clone();
        Box::pin(async move {
            for service in &services {
                ctx.provide(service, 1_u64).await?;
            }
            Ok(())
        })
    })
}

/// A consumer declaring every service in `services`.
fn consumer_plugin(name: &'static str, services: &[String]) -> Arc<dyn Plugin> {
    let injections: Vec<_> = services.iter().map(|s| Injection::new(s.clone())).collect();
    plugin(name, injections, |_ctx, _config| {
        Box::pin(async move { Ok(()) })
    })
}

/// Spawns one provider, retrying while the slot is still held by the
/// outgoing provider (`duplicate_provide` means the withdrawal has not
/// landed yet; any other startup error is a failure).
async fn spawn_provider_retried(ctx: &Context, provider: &Arc<dyn Plugin>) -> FiberHandle {
    for _attempt in 0..100 {
        let handle = ctx
            .plugin(Arc::clone(provider), None)
            .expect("spawn provider");
        match handle.join().await {
            Ok(()) => return handle,
            Err(err) => {
                assert_eq!(
                    err.code, "duplicate_provide",
                    "only the slot handoff may fail a replacement spawn"
                );
                handle.dispose().await;
            }
        }
    }
    panic!("provider slot never freed across 100 attempts");
}

/// Emits one machine-readable `BENCH` result line through the kernel logger.
fn report(
    ctx: &Context,
    name: &str,
    ops: usize,
    wall: Duration,
    (status, consumer_status, service): (u64, u64, u64),
) {
    ctx.logger().log(
        LogLevel::Info,
        "bench",
        &format!(
            "BENCH {name} ops={ops} wall_ms={} status_events={status} \
             consumer_status_events={consumer_status} service_events={service}",
            wall.as_millis(),
        ),
    );
}

/// Gives the P2 queue a bounded window to deliver the last counter events
/// before the delta is read.
async fn settle_counters() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// B1: same-fiber refresh storm. `CHURN_SERVICES` tasks concurrently replace
/// their slots while `CONSUMERS` consumers declare every churn slot plus one
/// stable anchor — every replacement notifies every consumer, and the
/// overlapping notifications land while consumers are mid-convergence, which
/// is exactly the same-target refresh churn the P3 coalescing removes.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run on demand with --ignored"]
async fn b1_same_target_refresh_storm() {
    let ctx = Context::new();
    let counters = Counters::new().install(&ctx, "b1.consumer").await;

    let churn: Vec<String> = CHURN_SLOTS.iter().map(|slot| (*slot).to_string()).collect();
    let mut declared = churn.clone();
    declared.push("b1.anchor".to_string());

    let mut consumers = Vec::new();
    for _ in 0..CONSUMERS {
        let consumer = consumer_plugin("b1.consumer", &declared);
        consumers.push(ctx.plugin(consumer, None).expect("spawn consumer"));
    }
    let anchor = provider_plugin("b1.anchor.plugin", "b1.anchor");
    let anchor = ctx.plugin(anchor, None).expect("spawn anchor");
    anchor.join().await.expect("anchor active");

    // One churn task per slot; slots are disjoint so no handoff contention.
    let mut tasks = Vec::new();
    for service in CHURN_SLOTS {
        let ctx = ctx.clone();
        let provider = provider_plugin("b1.churn.plugin", service);
        tasks.push(tokio::spawn(async move {
            let mut handle = spawn_provider_retried(&ctx, &provider).await;
            for _round in 0..ROUNDS {
                handle.dispose().await;
                handle = spawn_provider_retried(&ctx, &provider).await;
            }
            handle
        }));
    }

    let start = Instant::now();
    let mut final_handles = Vec::new();
    for task in tasks {
        final_handles.push(
            tokio::time::timeout(RAIL, task)
                .await
                .expect("bench stalled"),
        );
    }
    let _ = anchor.join().await;
    for consumer in &consumers {
        consumer.join().await.expect("consumer active after storm");
    }
    let wall = start.elapsed();
    settle_counters().await;
    let before = counters.snapshot();
    let ops = CHURN_SERVICES * ROUNDS * 2;
    report(&ctx, "b1_same_target_refresh_storm", ops, wall, before);

    let (status, consumer_status, service) = before;
    assert!(
        status > 0 && consumer_status > 0 && service > 0,
        "counters ran"
    );
    for consumer in &consumers {
        assert_eq!(
            consumer.state(),
            FiberState::Active,
            "consumer rode the storm"
        );
    }
    ctx.stop().await;
    drop(final_handles);
}

/// B2: multi-service withdrawal fan-out. One provider owns `GROUP_SERVICES`
/// slots; `CONSUMERS` consumers declare all of them. Every provider
/// replacement runs one withdrawal section per service plus the batched
/// reload notify — the S6-disciplined path that P3 deliberately does not
/// merge (findings §19 W2); the scenario pins its cost as a no-regression
/// anchor.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run on demand with --ignored"]
async fn b2_multi_service_withdraw_group() {
    let ctx = Context::new();
    let counters = Counters::new().install(&ctx, "b2.consumer").await;

    let services: Vec<String> = (0..GROUP_SERVICES)
        .map(|index| format!("b2.svc.{index}"))
        .collect();
    let declared = services.clone();

    let mut consumers = Vec::new();
    for _ in 0..CONSUMERS {
        let consumer = consumer_plugin("b2.consumer", &declared);
        consumers.push(ctx.plugin(consumer, None).expect("spawn consumer"));
    }
    let provider = group_provider_plugin("b2.group.plugin", services);
    let mut handle = ctx.plugin(provider, None).expect("spawn group provider");
    handle.join().await.expect("group provider active");
    for consumer in &consumers {
        consumer.join().await.expect("consumer active after setup");
    }

    let start = Instant::now();
    for _round in 0..ROUNDS {
        handle.dispose().await;
        let provider = group_provider_plugin("b2.group.plugin", declared.clone());
        handle = ctx.plugin(provider, None).expect("respawn group provider");
        handle.join().await.expect("group provider active");
    }
    for consumer in &consumers {
        consumer.join().await.expect("consumer active after storm");
    }
    let wall = start.elapsed();
    settle_counters().await;
    let snapshot = counters.snapshot();
    let ops = ROUNDS * 2;
    report(&ctx, "b2_multi_service_withdraw_group", ops, wall, snapshot);

    let (status, consumer_status, service) = snapshot;
    assert!(
        status > 0 && consumer_status > 0 && service > 0,
        "counters ran"
    );
    for consumer in &consumers {
        assert_eq!(
            consumer.state(),
            FiberState::Active,
            "consumer rode the storm"
        );
    }
    ctx.stop().await;
}

/// B3: transitive chain refresh. `A <- B <- C` dependency chain; replacing
/// A's provider propagates through B's withdrawal and re-provide into C.
/// Pins the per-round propagation cost of the notify chain (post-load batch
/// notify plus the apply-arm notify of the re-activated middle fiber).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run on demand with --ignored"]
async fn b3_chain_refresh_propagation() {
    let ctx = Context::new();
    let counters = Counters::new().install(&ctx, "b3.").await;

    let root = provider_plugin("b3.root.plugin", "b3.a");
    let mut root = ctx.plugin(root, None).expect("spawn root");
    root.join().await.expect("root active");
    let mid = plugin("b3.mid", vec![Injection::new("b3.a")], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("b3.b", 1_u64).await?;
            Ok(())
        })
    });
    let mid = ctx.plugin(mid, None).expect("spawn mid");
    mid.join().await.expect("mid active");
    let leaf = plugin("b3.leaf", vec![Injection::new("b3.b")], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("b3.c", 1_u64).await?;
            Ok(())
        })
    });
    let leaf = ctx.plugin(leaf, None).expect("spawn leaf");
    leaf.join().await.expect("leaf active");

    let provider = provider_plugin("b3.root.plugin", "b3.a");
    let start = Instant::now();
    for _round in 0..ROUNDS {
        root.dispose().await;
        root = spawn_provider_retried(&ctx, &provider).await;
        mid.join().await.expect("mid reconverged");
        leaf.join().await.expect("leaf reconverged");
    }
    let wall = start.elapsed();
    settle_counters().await;
    let snapshot = counters.snapshot();
    let ops = ROUNDS * 2;
    report(&ctx, "b3_chain_refresh_propagation", ops, wall, snapshot);

    let (status, _consumer_status, service) = snapshot;
    assert!(status > 0 && service > 0, "counters ran");
    assert_eq!(mid.state(), FiberState::Active, "mid settled Active");
    assert_eq!(leaf.state(), FiberState::Active, "leaf settled Active");
    ctx.stop().await;
}
