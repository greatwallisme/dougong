// Rust guideline compliant 2026-10-25
//! Plugin resource budgets (performance track P7, improve 一.7): per-fiber
//! caps on live effects and per-activation-window event emissions, configured
//! through `ContextOptions::budgets`. Every test exercises an opt-in cap; the
//! default (`BudgetConfig::default()`, all `None`) is pinned as a zero
//! behavior change by the entire existing suite, which runs unconfigured.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{
    BudgetConfig, Context, ContextOptions, EmitOutcome, FiberState, Injection, Listener, LogLevel,
    Logger, LoggerHandle, plugin,
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

/// A log sink capturing every message for assertions.
#[derive(Clone, Default)]
struct Capture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Capture {
    fn count(&self, needle: &str) -> usize {
        self.lines
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|line| line.contains(needle))
            .count()
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

fn budgets(max_effects: Option<u32>, max_event_emits: Option<u64>) -> ContextOptions {
    ContextOptions {
        budgets: BudgetConfig {
            max_effects,
            max_event_emits,
        },
        ..ContextOptions::default()
    }
}

/// A listener that counts deliveries into `counter`, registered by its own
/// plugin (listeners are fiber effects; the root context has no fiber).
async fn spawn_counter(ctx: &Context, name: &str, counter: Arc<AtomicUsize>) {
    let name = name.to_string();
    let listener: Listener = Arc::new(move |_ctx, _payload, _next| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(Value::Null)
        })
    });
    let counter_plugin = plugin("counter", vec![], move |ctx, _config| {
        let listener = Arc::clone(&listener);
        let name = name.clone();
        Box::pin(async move {
            ctx.on(&name, listener).await?;
            Ok(())
        })
    });
    ctx.plugin(counter_plugin, None)
        .expect("spawn counter plugin")
        .join()
        .await
        .expect("counter plugin active");
}

#[tokio::test]
async fn effect_cap_fails_load_and_rolls_back() {
    let ctx = Context::new_with_options(budgets(Some(2), None));
    let over = plugin("over-cap", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-a", 1_u32).await?;
            ctx.provide("svc-b", 2_u32).await?;
            // Third live effect on a cap of two: the body must fail.
            ctx.provide("svc-c", 3_u32).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(over, None).expect("spawn");
    let err = handle
        .join()
        .await
        .expect_err("effect cap must fail the activation");
    assert_eq!(err.code, "budget_exceeded");
    assert_eq!(handle.state(), FiberState::Failed);
    // Transactional rollback: no service registered by the failed attempt
    // may survive as residue.
    assert!(ctx.get::<u32>("svc-a").is_none());
    assert!(ctx.get::<u32>("svc-b").is_none());
    assert!(ctx.get::<u32>("svc-c").is_none());
    ctx.stop().await;
}

#[tokio::test]
async fn effect_cap_exact_boundary_is_allowed() {
    let ctx = Context::new_with_options(budgets(Some(3), None));
    let exact = plugin("at-cap", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-a", 1_u32).await?;
            ctx.provide("svc-b", 2_u32).await?;
            ctx.provide("svc-c", 3_u32).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(exact, None).expect("spawn");
    handle.join().await.expect("exactly at cap is allowed");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(handle.effect_count(), 3);
    ctx.stop().await;
}

#[tokio::test]
async fn effect_cap_covers_listener_registrations() {
    let ctx = Context::new_with_options(budgets(Some(1), None));
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_in_body = Arc::clone(&captured);
    let mixed = plugin("mixed", vec![], move |ctx, _config| {
        let captured = Arc::clone(&captured_in_body);
        Box::pin(async move {
            ctx.provide("only", 1_u32).await?;
            let listener: Listener =
                Arc::new(|_ctx, _payload, _next| Box::pin(async { Ok(Value::Null) }));
            // A listener registration is itself an effect (Critical teardown),
            // so the cap must reject it exactly like a provide.
            if let Err(err) = ctx.on("evt", listener).await {
                *captured.lock().expect("capture lock") = Some(err.code.to_string());
            }
            Ok(())
        })
    });
    let handle = ctx.plugin(mixed, None).expect("spawn");
    handle.join().await.expect("body swallows the breach");
    assert_eq!(
        captured.lock().expect("capture lock").as_deref(),
        Some("budget_exceeded")
    );
    ctx.stop().await;
}

#[tokio::test]
async fn emit_budget_drops_excess_and_logs_once() {
    let ctx = Context::new_with_options(budgets(None, Some(3)));
    let capture = Capture::default();
    let isolated = install_logger(&ctx, &capture).await;
    let delivered = Arc::new(AtomicUsize::new(0));
    spawn_counter(&isolated, "bench.evt", Arc::clone(&delivered)).await;

    let emitter = plugin("emitter", vec![], |ctx, _config| {
        Box::pin(async move {
            for _ in 0..5 {
                ctx.emit("bench.evt", Value::Null);
            }
            Ok(())
        })
    });
    isolated
        .plugin(emitter, None)
        .expect("spawn emitter")
        .join()
        .await
        .expect("emitter active");

    // Exactly the first three emissions of the window are delivered; the
    // breach is logged once, further breaches drop silently.
    wait_until(|| delivered.load(Ordering::Relaxed) == 3).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(delivered.load(Ordering::Relaxed), 3);
    assert_eq!(capture.count("event budget of 3 exceeded"), 1);
    ctx.stop().await;
}

#[tokio::test]
async fn emit_budget_window_resets_on_restart() {
    let ctx = Context::new_with_options(budgets(None, Some(2)));
    let delivered = Arc::new(AtomicUsize::new(0));
    spawn_counter(&ctx, "bench.re", Arc::clone(&delivered)).await;

    let emitter = plugin("emitter", vec![], |ctx, _config| {
        Box::pin(async move {
            for _ in 0..3 {
                ctx.emit("bench.re", Value::Null);
            }
            Ok(())
        })
    });
    let handle = ctx.plugin(emitter, None).expect("spawn");
    handle.join().await.expect("emitter active");
    wait_until(|| delivered.load(Ordering::Relaxed) == 2).await;

    // A fresh activation starts a fresh window: without the reset the second
    // window would stay at two deliveries and this wait would fail.
    handle.restart().await.expect("restart");
    wait_until(|| delivered.load(Ordering::Relaxed) == 4).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(delivered.load(Ordering::Relaxed), 4);
    ctx.stop().await;
}

#[tokio::test]
async fn emit_with_policy_reports_budget_refusal_as_dropped() {
    let ctx = Context::new_with_options(budgets(None, Some(1)));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let outcomes_in_body = Arc::clone(&outcomes);
    let emitter = plugin("emitter", vec![], move |ctx, _config| {
        let outcomes = Arc::clone(&outcomes_in_body);
        Box::pin(async move {
            let first = ctx.emit_with_policy("evt6", Value::Null).await;
            let second = ctx.emit_with_policy("evt6", Value::Null).await;
            outcomes.lock().expect("capture lock").push((first, second));
            Ok(())
        })
    });
    ctx.plugin(emitter, None)
        .expect("spawn emitter")
        .join()
        .await
        .expect("emitter active");
    assert_eq!(
        outcomes.lock().expect("capture lock").as_slice(),
        [(EmitOutcome::Queued, EmitOutcome::Dropped)]
    );
    ctx.stop().await;
}

#[tokio::test]
async fn kernel_withdrawal_emissions_exempt_from_budget() {
    let ctx = Context::new_with_options(budgets(None, Some(1)));
    let provider = plugin("provider", vec![], |ctx, _config| {
        Box::pin(async move {
            // Exhaust the one-event window: if kernel-protocol emissions were
            // budgeted, the withdrawal's `internal/service` emission below
            // would be dropped and the consumer would never be notified.
            ctx.emit("provider.warm", Value::Null);
            ctx.provide("db", "value".to_string()).await?;
            Ok(())
        })
    });
    let provider_handle = ctx.plugin(provider, None).expect("spawn");
    provider_handle.join().await.expect("provider active");

    let consumer = plugin("consumer", vec![Injection::new("db")], |ctx, _config| {
        Box::pin(async move {
            ctx.require::<String>("db")?;
            Ok(())
        })
    });
    let consumer_handle = ctx.plugin(consumer, None).expect("spawn");
    consumer_handle.join().await.expect("consumer active");

    // Withdrawal with an exhausted budget: the consumer must still converge
    // back to Pending (kernel `emit_internal` carries no admission check).
    provider_handle.dispose().await;
    wait_until(|| consumer_handle.state() == FiberState::Pending).await;
    ctx.stop().await;
}
