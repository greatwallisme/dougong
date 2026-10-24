// Rust guideline compliant 2026-10-24
//! Event-dispatch fan-out benchmark (performance track P2, improve 一.2).
//!
//! Run on demand with:
//!
//! ```sh
//! cargo test --release -p dougong --test bench_events -- --ignored --nocapture
//! ```
//!
//! The correctness anchor is exact delivery; wall time and peak concurrent
//! listener invocations are measurements, never timing assertions.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dougong::{Context, Listener, LogLevel};
use serde_json::Value;

const EVENTS: usize = 2_000;
const LISTENERS: usize = 8;
const YIELDS: usize = 2;
const RAIL: Duration = Duration::from_secs(180);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run on demand with --ignored"]
async fn emit_fanout_bounded_execution() {
    let ctx = Context::new();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    for _ in 0..LISTENERS {
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        let delivered = Arc::clone(&delivered);
        let listener: Listener = Arc::new(move |_ctx, _payload, _next| {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let delivered = Arc::clone(&delivered);
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                peak.fetch_max(current, Ordering::Relaxed);
                for _ in 0..YIELDS {
                    tokio::task::yield_now().await;
                }
                active.fetch_sub(1, Ordering::Relaxed);
                delivered.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Null)
            })
        });
        ctx.on("bench/fanout", listener)
            .await
            .expect("register benchmark listener");
    }

    let ops = EVENTS * LISTENERS;
    let start = Instant::now();
    for _ in 0..EVENTS {
        ctx.emit("bench/fanout", Value::Null);
    }
    tokio::time::timeout(RAIL, async {
        while delivered.load(Ordering::Relaxed) != ops {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("event fan-out stalled");
    let wall = start.elapsed();
    ctx.logger().log(
        LogLevel::Info,
        "bench",
        &format!(
            "BENCH emit_fanout ops={ops} wall_ms={} max_active={}",
            wall.as_millis(),
            peak.load(Ordering::Relaxed)
        ),
    );
    assert_eq!(delivered.load(Ordering::Relaxed), ops);
    ctx.stop().await;
}
