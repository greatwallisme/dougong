// Rust guideline compliant 2026-10-25
//! Runtime metrics (observability track O2, improve 三.2): always-on
//! counters, opt-in latency instrumentation, and the `Context::metrics`
//! snapshot face. The default configuration records no latency samples at
//! all, which these tests pin as the zero-overhead contract.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dougong::{
    ApplyPolicy, ApplyTimeout, Context, ContextOptions, EventPolicy, EventQueueConfig,
    MetricsConfig, OverflowPolicy, TimeoutConfig, plugin,
};
use serde_json::Value;

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: Duration = Duration::from_secs(5);

/// Polls the condition with a hard ceiling: a hang fails, never flakes.
async fn wait_until(condition: impl Fn() -> bool + Send + Sync) {
    tokio::time::timeout(RAIL, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition reached");
}

#[tokio::test]
async fn fiber_distribution_and_provider_count_follow_the_runtime() {
    let ctx = Context::new();
    let plug = plugin("counted", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("m-a", 1_u32).await?;
            ctx.provide("m-b", 2_u32).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");

    let snapshot = ctx.metrics();
    assert_eq!(snapshot.active_fibers, 1, "the counted fiber is active");
    // The root context seeds exactly one built-in provider (`logger`).
    assert_eq!(snapshot.provider_count, 3);
    assert_eq!(snapshot.live_fibers, snapshot.active_fibers);
    assert_eq!(snapshot.failed_fibers, 0);

    handle.dispose().await;
    let snapshot = ctx.metrics();
    assert_eq!(snapshot.active_fibers, 0);
    assert_eq!(snapshot.live_fibers, 0);
    ctx.stop().await;
}

#[tokio::test]
async fn always_on_counters_move_with_lifecycle_and_notify() {
    let ctx = Context::new();
    let before = ctx.metrics();
    assert_eq!(before.state_transition_count, 0);
    assert_eq!(before.notify_count, 0);

    let plug = plugin("ticker", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("t-svc", 1_u32).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");

    let mid = ctx.metrics();
    assert!(
        mid.state_transition_count >= 2,
        "activation runs at least two mirror changes (loading + active): {}",
        mid.state_transition_count
    );
    assert!(mid.notify_count >= 1, "the provide commit notifies");
    // A provider with no consumers wires no edges (the counter counts wiring
    // events, not provides) — a declared consumer commits the first edge.
    assert_eq!(mid.dependency_rebuild_count, 0);

    let consumer = plugin(
        "consumer",
        vec![dougong::Injection::new("t-svc")],
        |_ctx, _config| Box::pin(async { Ok(()) }),
    );
    let consumer_handle = ctx.plugin(consumer, None).expect("spawn consumer");
    consumer_handle.join().await.expect("consumer active");
    let wired = ctx.metrics();
    assert!(
        wired.dependency_rebuild_count >= 1,
        "consumer committed edges"
    );

    handle.dispose().await;
    let after = ctx.metrics();
    assert!(after.state_transition_count > wired.state_transition_count);
    assert!(
        after.dependency_rebuild_count > wired.dependency_rebuild_count,
        "withdrawal uncommits the edges"
    );
    ctx.stop().await;
}

#[tokio::test]
async fn latency_instrumentation_is_off_by_default() {
    let ctx = Context::new();
    let plug = plugin("plain", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    let _ = ctx.get::<LoggerGate>("logger");

    let snapshot = ctx.metrics();
    assert_eq!(snapshot.resolution.count, 0, "default records nothing");
    assert_eq!(snapshot.update.count, 0);
    assert_eq!(snapshot.cleanup.count, 0);
    assert_eq!(snapshot.drain.count, 0);
    ctx.stop().await;
}

/// Marker type for a typed `get` probe.
struct LoggerGate;

#[tokio::test]
async fn latency_instrumentation_records_when_enabled() {
    let ctx = Context::new_with_options(ContextOptions {
        metrics: MetricsConfig { latencies: true },
        ..ContextOptions::default()
    });
    let plug = plugin("timed", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("timed-svc", 1_u32).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    let _ = ctx.get::<u32>("timed-svc");

    let snapshot = ctx.metrics();
    assert!(snapshot.resolution.count >= 1, "get was timed");
    assert!(snapshot.resolution.total > Duration::ZERO);
    // Bucket invariants: the histogram must account for every sample.
    let bucketed: u64 = snapshot.resolution.buckets.iter().sum();
    assert_eq!(bucketed, snapshot.resolution.count);
    ctx.stop().await;
}

#[tokio::test]
async fn cleanup_latency_counts_disposers() {
    let ctx = Context::new_with_options(ContextOptions {
        metrics: MetricsConfig { latencies: true },
        ..ContextOptions::default()
    });
    let plug = plugin("clean", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.effect("note", async {
                let disposer: dougong::Disposer = Box::new(|| Box::pin(async {}));
                Ok(Some(disposer))
            })
            .await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    handle.dispose().await;

    let snapshot = ctx.metrics();
    assert_eq!(snapshot.cleanup.count, 1, "one disposer ran");
    ctx.stop().await;
}

#[tokio::test]
async fn apply_retries_and_timeout_class_are_counted() {
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig {
            apply: Some(ApplyTimeout {
                budget: Duration::from_millis(20),
                policy: ApplyPolicy::Retry(2),
            }),
            ..TimeoutConfig::default()
        },
        ..ContextOptions::default()
    });
    let plug = plugin("slow", vec![], |_ctx, _config| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        })
    });
    let handle = ctx.plugin(plug, None).expect("spawn");
    let err = handle
        .join()
        .await
        .expect_err("the budget fires after the retries");
    assert_eq!(err.code, "apply_timeout");

    let snapshot = ctx.metrics();
    assert_eq!(snapshot.apply_retry_count, 2, "two retries, then park");
    // Intermediate retry attempts record no error line — only the parked
    // fail_load constructs the apply_timeout error (retry churn is visible
    // in the retry counter, not the error stream).
    assert_eq!(snapshot.timeout_error_count, 1);
    assert_eq!(snapshot.failed_fibers, 1, "the slow fiber parked failed");
    ctx.stop().await;
}

#[tokio::test]
async fn dropped_events_count_capacity_refusals() {
    let blocked = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let ctx = Context::new_with_options(ContextOptions {
        events: EventQueueConfig {
            workers: NonZeroUsize::new(1).expect("nonzero"),
            queue_capacity: NonZeroUsize::new(1),
            policies: std::iter::once((
                "evt.drop".to_string(),
                EventPolicy {
                    overflow: OverflowPolicy::Drop,
                    max_concurrency: None,
                },
            ))
            .collect(),
        },
        ..ContextOptions::default()
    });
    let emitter = {
        let blocked = Arc::clone(&blocked);
        let release = Arc::clone(&release);
        plugin("emitter", vec![], move |ctx, _config| {
            let blocked = Arc::clone(&blocked);
            let release = Arc::clone(&release);
            Box::pin(async move {
                // First emission occupies the single worker (blocked until
                // released); the second fills capacity 1; the third must drop.
                let listener_blocked = Arc::clone(&blocked);
                let listener: dougong::Listener = Arc::new(move |_ctx, _p, _n| {
                    let blocked = Arc::clone(&listener_blocked);
                    let release = Arc::clone(&release);
                    Box::pin(async move {
                        blocked.store(true, Ordering::SeqCst);
                        while !release.load(Ordering::SeqCst) {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Ok(Value::Null)
                    })
                });
                ctx.on("evt.drop", listener).await?;
                ctx.emit("evt.drop", Value::Null);
                // Wait until the single worker is inside the listener: only
                // then is capacity 1 free for exactly one more pending job.
                while !blocked.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                ctx.emit("evt.drop", Value::Null);
                ctx.emit("evt.drop", Value::Null);
                Ok(())
            })
        })
    };
    let handle = ctx.plugin(emitter, None).expect("spawn");
    handle.join().await.expect("emitter active");
    wait_until(|| ctx.metrics().dropped_events >= 1).await;

    let snapshot = ctx.metrics();
    assert_eq!(snapshot.dropped_events, 1, "capacity 1 + full worker");
    release.store(true, Ordering::SeqCst);
    wait_until(|| ctx.metrics().queue_depth == 0).await;
    ctx.stop().await;
}

#[tokio::test]
async fn error_class_counters_split_by_class() {
    let ctx = Context::new();
    let plug = plugin("failing", vec![], |_ctx, _config| {
        Box::pin(async { Err(dougong::Error::msg("logic burst")) })
    });
    let handle = ctx.plugin(plug, None).expect("spawn");
    let _ = handle.join().await;

    let snapshot = ctx.metrics();
    assert!(snapshot.logic_error_count >= 1);
    ctx.stop().await;
}
