// Rust guideline compliant 2026-10-24
//! Timeout budgets and policies (stability track S3): every critical async
//! path — plugin body apply, listener execution, provider drain, effect
//! cleanup, and the cancel-scope reap watchdog — is boundable through
//! `Context::new_with_timeouts`, and the per-path policy (fail / retry /
//! log-and-continue / cancel-consumer) decides what a timeout does.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{
    ApplyTimeout, Context, DispatchListener, DrainTimeout, FiberHandle, FiberState, Injection,
    Listener, LogLevel, Logger, LoggerHandle, TimeoutConfig, plugin,
};
use serde_json::{Value, json};

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: Duration = Duration::from_secs(5);

/// Small real-time budget: timeout semantics only ever need the deadline to
/// fire eventually (the hung side never completes on its own), so a short
/// real duration is deterministic under load.
const BUDGET: Duration = Duration::from_millis(50);

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

/// A log sink capturing every message for assertions.
#[derive(Clone, Default)]
struct Capture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Capture {
    fn contains(&self, needle: &str) -> bool {
        self.lines
            .lock()
            .expect("capture lock")
            .iter()
            .any(|line| line.contains(needle))
    }
}

impl Logger for Capture {
    fn log(&self, _level: LogLevel, _target: &str, message: &str) {
        self.lines
            .lock()
            .expect("capture lock")
            .push(message.to_string());
    }
}

/// Installs `capture` as the `logger` service inside a fresh isolation label
/// and returns the isolated context the test must use for everything (the
/// kernel seeds the built-in stderr logger on the default label, so a global
/// replacement is impossible by design; isolation is the override mechanism).
async fn install_logger(ctx: &Context, capture: &Capture) -> Context {
    let isolated = ctx.isolate("logger");
    let capture = capture.clone();
    let plugin = plugin("test-logger", vec![], move |ctx, _config| {
        let capture = capture.clone();
        Box::pin(async move {
            ctx.provide("logger", LoggerHandle::new(Arc::new(capture)))
                .await?;
            Ok(())
        })
    });
    isolated
        .plugin(plugin, None)
        .expect("spawn logger plugin")
        .join()
        .await
        .expect("logger plugin active");
    isolated
}

/// A one-shot latch released by the test to unblock exactly one hung call.
struct Latch {
    release_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    wait_rx: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl Latch {
    fn new() -> (Arc<Self>, Arc<Self>) {
        let (release_tx, wait_rx) = tokio::sync::oneshot::channel();
        let latch = Arc::new(Self {
            release_tx: Mutex::new(Some(release_tx)),
            wait_rx: Mutex::new(Some(wait_rx)),
        });
        (Arc::clone(&latch), latch)
    }

    /// Takes the receiver for the single hung invocation.
    fn wait(&self) -> tokio::sync::oneshot::Receiver<()> {
        self.wait_rx
            .lock()
            .expect("latch lock")
            .take()
            .expect("one hung invocation per test")
    }

    /// Releases the hung invocation, if still armed.
    fn release(&self) {
        let tx = self.release_tx.lock().expect("latch lock").take();
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
    }
}

/// A plugin whose body signals its start and then hangs forever.
fn hung_body_plugin(started: Arc<AtomicBool>) -> Arc<dyn dougong::Plugin> {
    plugin("hung", vec![], move |_ctx, _config| {
        started.store(true, Ordering::SeqCst);
        Box::pin(async move { std::future::pending().await })
    })
}

/// A plugin that listens on `event`; each invocation runs `behavior`.
fn listener_plugin(
    name: &'static str,
    event: &'static str,
    behavior: impl Fn() -> dougong::BoxFuture<dougong::Result<Value>> + Send + Sync + 'static,
) -> Arc<dyn dougong::Plugin> {
    let behavior = Arc::new(behavior);
    plugin(name, vec![], move |ctx, _config| {
        let behavior = Arc::clone(&behavior);
        Box::pin(async move {
            let listener: Listener = Arc::new(move |_ctx, _payload, _next| behavior());
            ctx.on(event, listener).await?;
            Ok(())
        })
    })
}

/// A plugin that bridges `event` across a (simulated) boundary; each
/// invocation runs `behavior` as a tracked call.
fn bridge_plugin(
    name: &'static str,
    event: &'static str,
    behavior: impl Fn() -> dougong::BoxFuture<dougong::Result<Value>> + Send + Sync + 'static,
) -> Arc<dyn dougong::Plugin> {
    let behavior = Arc::new(behavior);
    plugin(name, vec![], move |ctx, _config| {
        let behavior = Arc::clone(&behavior);
        Box::pin(async move {
            let bridge: DispatchListener = Arc::new(move |_ctx, _payload, _next, _mode| behavior());
            ctx.on_dispatch(event, bridge).await?;
            Ok(())
        })
    })
}

#[tokio::test]
async fn apply_timeout_fails_the_plugin_and_the_runtime_survives() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        apply: Some(ApplyTimeout::fail_after(BUDGET)),
        ..TimeoutConfig::default()
    });
    let started = Arc::new(AtomicBool::new(false));
    let hung = ctx
        .plugin(hung_body_plugin(Arc::clone(&started)), None)
        .expect("spawn hung plugin");
    wait_until(|| started.load(Ordering::SeqCst)).await;
    let err = hung
        .join()
        .await
        .expect_err("apply budget turns the hung body into a startup failure");
    assert_eq!(err.code, "apply_timeout");
    assert_eq!(hung.state(), FiberState::Failed);

    // The runtime is alive and healthy: an unrelated plugin still activates
    // and its service is resolvable.
    let healthy = plugin("healthy", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc", 42_i32).await?;
            Ok(())
        })
    });
    ctx.plugin(healthy, None)
        .expect("spawn healthy plugin")
        .join()
        .await
        .expect("healthy plugin active after the timeout");
    let value = ctx.require::<i32>("svc").expect("service resolves");
    assert_eq!(*value, 42);
}

#[tokio::test]
async fn apply_retry_recovers_once_the_body_stops_hanging() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        apply: Some(ApplyTimeout::retry_after(BUDGET, 2)),
        ..TimeoutConfig::default()
    });
    let attempts = Arc::new(AtomicUsize::new(0));
    let flaky = plugin("flaky", vec![], {
        let attempts = Arc::clone(&attempts);
        move |_ctx, _config| {
            let attempts = Arc::clone(&attempts);
            Box::pin(async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    });
    let handle = ctx.plugin(flaky, None).expect("spawn flaky plugin");
    tokio::time::timeout(RAIL, handle.join())
        .await
        .expect("join settles")
        .expect("second attempt succeeds");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn apply_retry_exhaustion_parks_failed() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        apply: Some(ApplyTimeout::retry_after(BUDGET, 1)),
        ..TimeoutConfig::default()
    });
    let attempts = Arc::new(AtomicUsize::new(0));
    let always = plugin("always-hung", vec![], {
        let attempts = Arc::clone(&attempts);
        move |_ctx, _config| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { std::future::pending().await })
        }
    });
    let handle = ctx.plugin(always, None).expect("spawn plugin");
    let err = tokio::time::timeout(RAIL, handle.join())
        .await
        .expect("join settles")
        .expect_err("exhausted retries park the fiber Failed");
    assert_eq!(err.code, "apply_timeout");
    assert_eq!(handle.state(), FiberState::Failed);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn parallel_counts_a_plain_listener_timeout_as_failure() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        listener: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let hung = listener_plugin("hung-listener", "evt", || {
        Box::pin(async move { std::future::pending().await })
    });
    let fast = listener_plugin("fast-listener", "evt", || {
        Box::pin(async move { Ok(json!("fast")) })
    });
    let hung = ctx.plugin(hung, None).expect("spawn hung listener");
    let fast = ctx.plugin(fast, None).expect("spawn fast listener");
    hung.join().await.expect("hung listener active");
    fast.join().await.expect("fast listener active");
    let err = tokio::time::timeout(RAIL, ctx.parallel("evt", Value::Null))
        .await
        .expect("dispatch settles")
        .expect_err("the timed-out listener joins the aggregate");
    assert!(
        err.to_string().contains("listener_timeout"),
        "aggregate names the timeout: {err}"
    );
}

#[tokio::test]
async fn parallel_bridge_timeout_detaches_and_the_call_survives() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        listener: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let (latch, hung_at) = Latch::new();
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let bridge = bridge_plugin("bridge-listener", "evt", {
        let started = Arc::clone(&started);
        let finished = Arc::clone(&finished);
        move || {
            started.store(true, Ordering::SeqCst);
            let finished = Arc::clone(&finished);
            let waiting = hung_at.wait();
            Box::pin(async move {
                waiting.await.expect("release latch");
                finished.store(true, Ordering::SeqCst);
                Ok(Value::Null)
            })
        }
    });
    let handle = ctx.plugin(bridge, None).expect("spawn bridge");
    handle.join().await.expect("bridge active");
    let err = tokio::time::timeout(RAIL, ctx.parallel("evt", Value::Null))
        .await
        .expect("dispatch settles at the budget")
        .expect_err("the timed-out bridge counts as a failure");
    assert!(
        err.to_string().contains("listener_timeout"),
        "aggregate names the timeout: {err}"
    );
    assert!(started.load(Ordering::SeqCst));
    // The tracked foreign call was never dropped: it completes once the
    // foreign side answers, and the unload reap accounts for it.
    assert!(!finished.load(Ordering::SeqCst));
    latch.release();
    wait_until(|| finished.load(Ordering::SeqCst)).await;
    tokio::time::timeout(RAIL, handle.dispose())
        .await
        .expect("dispose settles after the call completed");
}

#[tokio::test]
async fn serial_propagates_a_listener_timeout() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        listener: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let hung = listener_plugin("hung-listener", "evt", || {
        Box::pin(async move { std::future::pending().await })
    });
    let hung = ctx.plugin(hung, None).expect("spawn hung listener");
    hung.join().await.expect("hung listener active");
    let err = tokio::time::timeout(RAIL, ctx.serial("evt", Value::Null))
        .await
        .expect("dispatch settles")
        .expect_err("the timeout propagates");
    assert_eq!(err.code, "listener_timeout");
}

#[tokio::test]
async fn waterfall_bridge_timeout_propagates_without_calling_the_fallback() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        listener: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let (latch, hung_at) = Latch::new();
    let finished = Arc::new(AtomicBool::new(false));
    let bridge = bridge_plugin("bridge-listener", "evt", {
        let finished = Arc::clone(&finished);
        move || {
            let finished = Arc::clone(&finished);
            let waiting = hung_at.wait();
            Box::pin(async move {
                waiting.await.expect("release latch");
                finished.store(true, Ordering::SeqCst);
                Ok(Value::Null)
            })
        }
    });
    let handle = ctx.plugin(bridge, None).expect("spawn bridge");
    handle.join().await.expect("bridge active");
    let fallback_called = Arc::new(AtomicBool::new(false));
    let err = tokio::time::timeout(RAIL, async {
        ctx.waterfall("evt", Value::Null, {
            let fallback_called = Arc::clone(&fallback_called);
            move |payload| {
                fallback_called.store(true, Ordering::SeqCst);
                Box::pin(async move { Ok(payload) })
            }
        })
        .await
    })
    .await
    .expect("dispatch settles")
    .expect_err("the bridge timeout propagates");
    assert_eq!(err.code, "listener_timeout");
    assert!(
        !fallback_called.load(Ordering::SeqCst),
        "a timed-out listener fails the chain instead of reaching the fallback"
    );
    latch.release();
    wait_until(|| finished.load(Ordering::SeqCst)).await;
    tokio::time::timeout(RAIL, handle.dispose())
        .await
        .expect("dispose settles");
}

#[tokio::test]
async fn emit_logs_a_plain_listener_timeout() {
    let capture = Capture::default();
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        listener: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let ctx = install_logger(&ctx, &capture).await;
    let hung = listener_plugin("hung-listener", "evt", || {
        Box::pin(async move { std::future::pending().await })
    });
    let hung = ctx.plugin(hung, None).expect("spawn hung listener");
    hung.join().await.expect("hung listener active");
    ctx.emit("evt", Value::Null);
    wait_until(|| capture.contains("listener_timeout")).await;
}

/// Withdrawal-drain test rig: a provider of `svc` and a consumer whose
/// second activation hangs forever.
struct DrainRig {
    provider: FiberHandle,
    consumer: FiberHandle,
}

/// Builds the rig: the consumer's first body run succeeds, every later one
/// parks forever. The provider is joined *before* the consumer is spawned, so
/// the consumer's first join already observes `svc` and lands `Active` (a
/// consumer joined while its dependency is missing converges to `Pending`
/// instead, which would make the provider's later restart a no-op).
async fn drain_rig(ctx: &Context) -> DrainRig {
    let provider = plugin("provider", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc", 1_i32).await?;
            Ok(())
        })
    });
    let provider = ctx.plugin(provider, None).expect("spawn provider");
    provider.join().await.expect("provider active");
    let attempts = Arc::new(AtomicUsize::new(0));
    let consumer = plugin("consumer", vec![Injection::new("svc")], {
        let attempts = Arc::clone(&attempts);
        move |_ctx, _config| {
            let attempts = Arc::clone(&attempts);
            Box::pin(async move {
                if attempts.fetch_add(1, Ordering::SeqCst) > 0 {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    });
    let consumer = ctx.plugin(consumer, None).expect("spawn consumer");
    consumer.join().await.expect("consumer active");
    DrainRig { provider, consumer }
}

/// Reloads the provider so the consumer starts a second (hung) activation.
async fn rearm_hung_consumer(rig: &DrainRig) {
    tokio::time::timeout(RAIL, rig.provider.restart())
        .await
        .expect("provider restart settles")
        .expect("provider reloads");
    wait_until(|| rig.consumer.state() == FiberState::Loading).await;
}

#[tokio::test]
async fn drain_timeout_logs_and_completes_the_withdrawal() {
    let capture = Capture::default();
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        drain: Some(DrainTimeout::log_after(BUDGET)),
        ..TimeoutConfig::default()
    });
    let ctx = install_logger(&ctx, &capture).await;
    let rig = drain_rig(&ctx).await;
    rearm_hung_consumer(&rig).await;
    tokio::time::timeout(RAIL, rig.provider.dispose())
        .await
        .expect("provider withdrawal completes despite the stuck consumer");
    wait_until(|| capture.contains("drain_timeout")).await;
    // The consumer did not converge: its desired epoch is already inactive
    // (the withdrawal notified it) but its driver is stuck in the hung body.
    assert_eq!(rig.consumer.state(), FiberState::Unloading);
    // Cleanup: disposing the stuck consumer cancels its hung body.
    tokio::time::timeout(RAIL, rig.consumer.dispose())
        .await
        .expect("consumer dispose settles");
}

#[tokio::test]
async fn drain_timeout_cancel_consumer_lands_the_stuck_consumer_failed() {
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        drain: Some(DrainTimeout::cancel_after(BUDGET)),
        ..TimeoutConfig::default()
    });
    let rig = drain_rig(&ctx).await;
    rearm_hung_consumer(&rig).await;
    tokio::time::timeout(RAIL, rig.provider.dispose())
        .await
        .expect("provider withdrawal completes");
    wait_until(|| rig.consumer.state() == FiberState::Failed).await;
}

#[tokio::test]
async fn cleanup_timeout_orphans_a_hung_disposer_and_continues_lifo() {
    let capture = Capture::default();
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        cleanup: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let ctx = install_logger(&ctx, &capture).await;
    let (latch, hung_at) = Latch::new();
    let orphan_finished = Arc::new(AtomicBool::new(false));
    let fast_ran = Arc::new(AtomicBool::new(false));
    let effectful = plugin("effectful", vec![], {
        let orphan_finished = Arc::clone(&orphan_finished);
        let fast_ran = Arc::clone(&fast_ran);
        move |ctx, _config| {
            let orphan_finished = Arc::clone(&orphan_finished);
            let fast_ran = Arc::clone(&fast_ran);
            let hung_at = Arc::clone(&hung_at);
            Box::pin(async move {
                // Registered first, drained last: the hung disposer.
                ctx.effect("hung-teardown", async move {
                    let disposer: dougong::Disposer = Box::new(move || {
                        let waiting = hung_at.wait();
                        Box::pin(async move {
                            waiting.await.expect("release latch");
                            orphan_finished.store(true, Ordering::SeqCst);
                        })
                    });
                    Ok(Some(disposer))
                })
                .await?;
                // Registered second, drained first (LIFO): must still run.
                ctx.effect("fast-teardown", async move {
                    let disposer: dougong::Disposer = Box::new(move || {
                        Box::pin(async move {
                            fast_ran.store(true, Ordering::SeqCst);
                        })
                    });
                    Ok(Some(disposer))
                })
                .await?;
                Ok(())
            })
        }
    });
    let handle = ctx.plugin(effectful, None).expect("spawn effectful plugin");
    handle.join().await.expect("effectful active");
    tokio::time::timeout(RAIL, handle.dispose())
        .await
        .expect("unload completes despite the orphaned disposer");
    assert!(fast_ran.load(Ordering::SeqCst));
    assert!(!orphan_finished.load(Ordering::SeqCst));
    wait_until(|| capture.contains("cleanup_timeout")).await;
    // The orphan was never aborted: releasing the foreign side lets it run
    // to completion in the background.
    latch.release();
    wait_until(|| orphan_finished.load(Ordering::SeqCst)).await;
}

#[tokio::test]
async fn reap_watchdog_reports_a_stuck_tracked_call_then_settles() {
    let capture = Capture::default();
    let ctx = Context::new_with_timeouts(TimeoutConfig {
        reap_watchdog: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let ctx = install_logger(&ctx, &capture).await;
    let (latch, hung_at) = Latch::new();
    let started = Arc::new(AtomicBool::new(false));
    let bridge = bridge_plugin("bridge-listener", "evt", {
        let started = Arc::clone(&started);
        move || {
            started.store(true, Ordering::SeqCst);
            let waiting = hung_at.wait();
            Box::pin(async move {
                waiting.await.expect("release latch");
                Ok(Value::Null)
            })
        }
    });
    let handle = ctx.plugin(bridge, None).expect("spawn bridge");
    handle.join().await.expect("bridge active");
    ctx.emit("evt", Value::Null);
    wait_until(|| started.load(Ordering::SeqCst)).await;
    let disposing = tokio::spawn({
        let handle = handle.clone();
        async move { handle.dispose().await }
    });
    // The reap cannot proceed while the tracked call is in flight; the
    // watchdog must report instead of proceeding to teardown.
    wait_until(|| capture.contains("reap_blocked")).await;
    latch.release();
    tokio::time::timeout(RAIL, disposing)
        .await
        .expect("dispose task settles")
        .expect("dispose joined");
    assert_eq!(handle.state(), FiberState::Disposed);
}
