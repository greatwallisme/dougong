// Rust guideline compliant 2026-10-25
//! Health snapshot (observability track O5, improve 三.5): `Context::health`
//! turns the counts of the O2 face into an actionable endpoint body — failed
//! fibers with their startup errors, the recent-errors ring fed at the S5
//! emit funnel, in-flight tracked boundary calls, and stale provider slots.

use std::sync::Arc;
use std::time::Duration;

use dougong::{Context, LoggerHandle, plugin};
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

/// Quiet sink: keeps kernel log lines out of the test output.
#[derive(Clone, Default)]
struct Quiet;

impl dougong::Logger for Quiet {
    fn log(&self, _level: dougong::LogLevel, _target: &str, _message: &str) {}
}

/// Installs `logger` as the `logger` service in a fresh isolation label.
async fn isolate_with(ctx: &Context, logger: Arc<dyn dougong::Logger>) -> Context {
    let isolated = ctx.isolate("logger");
    let plug = plugin("test-logger", vec![], move |ctx, _config| {
        let handle = LoggerHandle::new(Arc::clone(&logger));
        Box::pin(async move {
            ctx.provide("logger", handle).await?;
            Ok(())
        })
    });
    isolated
        .plugin(plug, None)
        .expect("spawn logger plugin")
        .join()
        .await
        .expect("logger plugin active");
    isolated
}

#[tokio::test]
async fn healthy_world_reports_counts_and_empty_details() {
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(Quiet)).await;

    let plug = plugin("healthy", vec![], |_ctx, _config| {
        Box::pin(async { Ok(()) })
    });
    isolated
        .plugin(plug, None)
        .expect("spawn")
        .join()
        .await
        .expect("active");

    let health = isolated.health();
    // The logger plugin and the healthy plugin: two runtimes, two actives.
    assert_eq!(health.plugin_count, 2);
    assert_eq!(health.active_fibers, 2);
    assert_eq!(health.live_fibers, 2);
    assert!(health.failed.is_empty(), "no failed fibers");
    assert!(health.recent_errors.is_empty(), "no graded errors yet");
    assert!(health.long_tasks.is_empty(), "no tracked calls in flight");
    assert!(health.stale_providers.is_empty(), "all slots resolve");
    assert!(health.healthy, "the default judgment is healthy");
    isolated.stop().await;
}

#[tokio::test]
async fn failed_fiber_lands_in_details_and_the_error_ring() {
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(Quiet)).await;

    let doomed = plugin("doomed", vec![], |_ctx, _config| {
        Box::pin(async { Err(dougong::Error::msg("startup exploded")) })
    });
    let handle = isolated.plugin(doomed, None).expect("spawn");
    assert!(handle.join().await.is_err());

    let health = isolated.health();
    assert_eq!(health.failed_fibers, 1);
    let failed = health.failed.first().expect("failed fiber detail");
    assert_eq!(failed.name, "doomed");
    assert!(failed.error.contains("startup exploded"));
    assert!(
        !health.healthy,
        "a failed fiber breaks the default judgment"
    );
    // The fail_load record was graded at the S5 funnel, so the ring saw it —
    // and it carries the same trace id the detail list reports.
    let ring_entry = health
        .recent_errors
        .iter()
        .find(|record| record.event == "fail_load")
        .expect("the ring captured the graded failure");
    assert!(ring_entry.message.contains("startup exploded"));
    assert_eq!(
        failed.trace.to_string(),
        ring_entry.trace.expect("ring trace").to_string(),
        "failed detail and ring entry join on the O3 trace"
    );
    // Rollback removed the failed body's provides, so nothing is stale.
    assert!(health.stale_providers.is_empty());
    isolated.stop().await;
}

#[tokio::test]
async fn error_ring_caps_at_64_and_drops_the_oldest() {
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(Quiet)).await;

    // 70 graded failures against the 64-entry ring: the first six must be
    // evicted (the cap is RECENT_ERROR_CAP in metrics.rs).
    for i in 0..70 {
        let doomed = plugin("doomed-ring", vec![], move |_ctx, _config| {
            Box::pin(async move { Err(dougong::Error::msg(format!("blast {i}"))) })
        });
        let handle = isolated.plugin(doomed, None).expect("spawn");
        assert!(handle.join().await.is_err());
    }

    let health = isolated.health();
    // Graded failure messages carry the class prefix ("internal: blast N"),
    // so match on the suffix.
    let ring: Vec<String> = health
        .recent_errors
        .iter()
        .filter(|record| record.event == "fail_load" && record.message.contains("blast "))
        .map(|record| record.message.clone())
        .collect();
    assert_eq!(ring.len(), 64, "the ring retains exactly the cap");
    assert!(
        ring.first().expect("oldest kept").ends_with("blast 6"),
        "the six oldest entries were evicted, got {:?}",
        ring.first()
    );
    assert!(ring.last().expect("newest kept").ends_with("blast 69"));
    isolated.stop().await;
}

#[tokio::test]
async fn in_flight_bridge_call_reports_as_long_task() {
    let ctx = Context::new_with_options(dougong::ContextOptions {
        timeouts: dougong::TimeoutConfig {
            // A listener budget routes bridge dispatches through the tracked
            // spawn (S3): exactly the work the long-task face watches.
            listener: Some(Duration::from_secs(5)),
            ..dougong::TimeoutConfig::default()
        },
        ..dougong::ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, Arc::new(Quiet)).await;

    let holder = plugin("holder", vec![], |ctx, _config| {
        Box::pin(async move {
            let bridge: dougong::DispatchListener =
                Arc::new(move |_ctx, _payload, _next, _mode| {
                    Box::pin(async {
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        Ok(Value::Null)
                    })
                });
            ctx.on_dispatch("evt.slow", bridge).await?;
            Ok(())
        })
    });
    let spawned = isolated.plugin(holder, None).expect("spawn");
    spawned.join().await.expect("holder active");

    isolated.emit("evt.slow", Value::Null);
    wait_until(|| !isolated.health().long_tasks.is_empty()).await;
    let health = isolated.health();
    let busy = health.long_tasks.first().expect("tracked holder listed");
    assert_eq!(busy.name, "holder");
    assert!(
        busy.longest > Duration::ZERO,
        "the age grows while in flight"
    );
    assert!(busy.trace > 0, "the entry joins on the O3 trace");
    wait_until(|| isolated.health().long_tasks.is_empty()).await;
    isolated.stop().await;
}
