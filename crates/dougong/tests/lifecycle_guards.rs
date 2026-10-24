// Rust guideline compliant 2026-10-24
//! Lifecycle API idempotency guards (stability track S4): `dispose` is
//! single-winner idempotent — including under concurrency — `restart` and
//! `update` fail with explicit reasons on disposed fibers, and `update` is
//! rejected while a transition (loading/unloading) is in flight.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, Disposer, EffectGuard, FiberState, Injection, plugin};
use serde_json::{Value, json};

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

/// A plugin whose single teardown effect counts how often it ran.
fn counting_plugin(name: &'static str, teardowns: Arc<AtomicUsize>) -> Arc<dyn dougong::Plugin> {
    plugin(name, vec![], move |ctx, _config| {
        let teardowns = Arc::clone(&teardowns);
        Box::pin(async move {
            let disposer: Disposer = Box::new(move || {
                Box::pin(async move {
                    teardowns.fetch_add(1, Ordering::Relaxed);
                })
            });
            ctx.effect("counter", async move { Ok(Some(disposer)) })
                .await?;
            Ok(())
        })
    })
}

/// A plugin that activates instantly and does nothing.
fn ok_plugin(name: &'static str) -> Arc<dyn dougong::Plugin> {
    plugin(name, vec![], |_ctx, _config| {
        Box::pin(async move { Ok(()) })
    })
}

#[tokio::test]
async fn dispose_is_idempotent_sequential() {
    let ctx = Context::new();
    let disposed = Arc::new(AtomicUsize::new(0));
    let fiber = ctx
        .plugin(counting_plugin("once", Arc::clone(&disposed)), None)
        .expect("spawn");
    fiber.join().await.expect("active");
    fiber.dispose().await;
    // A second call observes the same terminal state and never re-runs the
    // teardown.
    fiber.dispose().await;
    assert_eq!(disposed.load(Ordering::Relaxed), 1);
    assert_eq!(fiber.state(), FiberState::Disposed);
}

#[tokio::test]
async fn concurrent_dispose_runs_teardown_once() {
    let ctx = Context::new();
    // Count internal/plugin "disposed" notifications: exactly one may fire no
    // matter how many callers race the dispose (no duplicate notify, S4).
    let events = Arc::new(AtomicUsize::new(0));
    let events_listener = Arc::clone(&events);
    ctx.on(
        "internal/plugin",
        Arc::new(move |_ctx, payload, _next| {
            let events = Arc::clone(&events_listener);
            Box::pin(async move {
                if payload.get("event").and_then(Value::as_str) == Some("disposed") {
                    events.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Value::Null)
            })
        }),
    )
    .await
    .expect("listen");

    let disposed = Arc::new(AtomicUsize::new(0));
    let fiber = ctx
        .plugin(counting_plugin("raced", Arc::clone(&disposed)), None)
        .expect("spawn");
    fiber.join().await.expect("active");

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let fiber = fiber.clone();
        tasks.push(tokio::spawn(async move {
            fiber.dispose().await;
        }));
    }
    for task in tasks {
        task.await.expect("dispose task");
    }

    assert_eq!(disposed.load(Ordering::Relaxed), 1);
    assert_eq!(fiber.state(), FiberState::Disposed);
    // Dispatch is fire-and-forget: flush the queue, then the count must be
    // exactly one — a second winning teardown would have enqueued its own
    // notification before its caller returned.
    wait_until(|| events.load(Ordering::Relaxed) >= 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(events.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn restart_on_disposed_fails_with_explicit_reason() {
    let ctx = Context::new();
    let fiber = ctx.plugin(ok_plugin("doomed"), None).expect("spawn");
    fiber.join().await.expect("active");
    fiber.dispose().await;
    let err = fiber
        .restart()
        .await
        .expect_err("disposed restart must fail");
    assert_eq!(err.code, "fiber_disposed");
}

#[tokio::test]
async fn update_on_disposed_fails_with_explicit_reason() {
    let ctx = Context::new();
    let fiber = ctx.plugin(ok_plugin("doomed"), None).expect("spawn");
    fiber.join().await.expect("active");
    fiber.dispose().await;
    let err = fiber
        .update(json!({}))
        .await
        .expect_err("disposed update must fail");
    assert_eq!(err.code, "fiber_disposed");
}

#[tokio::test]
async fn update_on_unloading_fails_with_explicit_reason() {
    let ctx = Context::new();
    // The provider stashes its provide guard so the test can withdraw later.
    let guard_slot: Arc<Mutex<Option<EffectGuard>>> = Arc::new(Mutex::new(None));
    let guard_slot_in_body = Arc::clone(&guard_slot);
    let provider = plugin("provider", vec![], move |ctx, _config| {
        let guard_slot = Arc::clone(&guard_slot_in_body);
        Box::pin(async move {
            let guard = ctx.provide("svc", 42_i32).await?;
            *guard_slot.lock().expect("guard slot") = Some(guard);
            Ok(())
        })
    });
    ctx.plugin(provider, None)
        .expect("spawn provider")
        .join()
        .await
        .expect("provider active");

    // The consumer's teardown blocks on a one-shot gate, so the provider
    // withdrawal parks it in Unloading until the test releases the gate.
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
    let gate_slot: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>> =
        Arc::new(Mutex::new(Some(gate_rx)));
    let consumer = plugin(
        "consumer",
        vec![Injection::new("svc")],
        move |ctx, _config| {
            let gate_slot = Arc::clone(&gate_slot);
            Box::pin(async move {
                let gate = gate_slot.lock().expect("gate slot").take();
                let disposer: Disposer = Box::new(move || {
                    Box::pin(async move {
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                    })
                });
                ctx.effect("gate", async move { Ok(Some(disposer)) })
                    .await?;
                Ok(())
            })
        },
    );
    let consumer = ctx.plugin(consumer, None).expect("spawn consumer");
    consumer.join().await.expect("consumer active");

    let guard = guard_slot
        .lock()
        .expect("guard slot")
        .take()
        .expect("provide guard");
    // Draining the consumer waits on the gated disposer, so the withdrawal
    // must run on its own task while the test observes the consumer.
    let withdraw = tokio::spawn(async move { guard.dispose().await });
    wait_until(|| consumer.state() == FiberState::Unloading).await;

    let err = consumer
        .update(json!({}))
        .await
        .expect_err("unloading update must be rejected");
    assert_eq!(err.code, "fiber_unloading");

    gate_tx.send(()).expect("open gate");
    withdraw.await.expect("withdraw task").expect("withdraw");
    // The consumer settles parked: its dependency is gone for good.
    wait_until(|| consumer.state() == FiberState::Pending).await;
}

#[tokio::test]
async fn update_on_loading_fails_with_explicit_reason() {
    let ctx = Context::new();
    // The body blocks on a one-shot gate, holding the fiber in Loading.
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
    let gate_slot: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>> =
        Arc::new(Mutex::new(Some(gate_rx)));
    let slow = plugin("slow", vec![], move |_ctx, _config| {
        let gate = gate_slot.lock().expect("gate slot").take();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Ok(())
        })
    });
    let fiber = ctx.plugin(slow, None).expect("spawn");
    wait_until(|| fiber.state() == FiberState::Loading).await;

    let err = fiber
        .update(json!({}))
        .await
        .expect_err("loading update must be rejected");
    assert_eq!(err.code, "fiber_loading");

    gate_tx.send(()).expect("open gate");
    fiber.join().await.expect("active");
}
