// Rust guideline compliant 2026-10-24
//! Fault-injection suite (stability track S7, improve requirement 二.7):
//! repeated/hot updates, concurrent unload+inject+event races, dynamic
//! realm changes, and panicking listeners. Scenarios already saturated by
//! earlier tracks (multiple provider changes — S6 ordering storms; provider
//! drain timeout — S3 timeout budgets) are intentionally not duplicated.
//!
//! Every test runs the full debug-tier invariant checkpoints (S1) as its
//! structural tripwire: any store/index/scope residue left by a fault
//! panics with a locating snapshot instead of passing silently.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, FiberHandle, FiberState, Injection, plugin};
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

/// A plugin recording every config its body runs with (one entry per
/// activation — `update` is a restart-style hot update).
fn config_recorder(log: Arc<Mutex<Vec<Value>>>) -> Arc<dyn dougong::Plugin> {
    plugin("config-recorder", vec![], move |_ctx, config| {
        let log = Arc::clone(&log);
        Box::pin(async move {
            log.lock().expect("log").push(config);
            Ok(())
        })
    })
}

/// A provider of `service` carrying the given value marker.
fn provider_plugin(
    service: &'static str,
    value: i32,
    invocations: Arc<AtomicUsize>,
) -> Arc<dyn dougong::Plugin> {
    plugin("provider", vec![], move |ctx, _config| {
        let invocations = Arc::clone(&invocations);
        Box::pin(async move {
            ctx.provide(service, value).await?;
            ctx.on(
                "tick",
                Arc::new(move |_ctx, _payload, _next| {
                    invocations.fetch_add(1, AtomicOrdering::SeqCst);
                    Box::pin(async move { Ok(Value::Null) })
                }),
            )
            .await?;
            Ok(())
        })
    })
}

/// Spawns a provider, retrying while the slot is still held by the outgoing
/// provider (`duplicate_provide` means the withdrawal has not landed yet).
async fn spawn_provider_retried(
    ctx: &Context,
    service: &'static str,
    value: i32,
    invocations: Arc<AtomicUsize>,
) -> FiberHandle {
    let attempts = AtomicUsize::new(0);
    loop {
        let handle = ctx
            .plugin(
                provider_plugin(service, value, Arc::clone(&invocations)),
                None,
            )
            .expect("spawn provider");
        match handle.join().await {
            Ok(()) => return handle,
            Err(err) => {
                assert_eq!(
                    err.code, "duplicate_provide",
                    "only the slot handoff may fail a replacement spawn"
                );
                handle.dispose().await;
                assert!(
                    attempts.fetch_add(1, AtomicOrdering::Relaxed) < 100,
                    "slot never freed"
                );
            }
        }
    }
}

/// A consumer recording every activation's resolved value.
fn recording_consumer(
    service: &'static str,
    log: Arc<Mutex<Vec<i32>>>,
) -> Arc<dyn dougong::Plugin> {
    plugin(
        "consumer",
        vec![Injection::new(service)],
        move |ctx, _config| {
            let log = Arc::clone(&log);
            Box::pin(async move {
                let value = ctx.require::<i32>(service)?;
                log.lock().expect("log").push(*value);
                Ok(())
            })
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_updates_land_exactly() {
    let ctx = Context::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = ctx
        .plugin(config_recorder(Arc::clone(&log)), Some(json!({"v": 0})))
        .expect("spawn recorder");
    handle.join().await.expect("initial activation");

    // Repeated hot reload: every accepted update is one restart with exactly
    // that config, so the observed sequence is exact, not just convergent.
    for round in 1..=20_i32 {
        handle
            .update(json!({"v": round}))
            .await
            .expect("update lands");
    }

    let expected: Vec<Value> = (0..=20_i32).map(|v| json!({"v": v})).collect();
    assert_eq!(*log.lock().expect("log"), expected);
    assert_eq!(handle.state(), FiberState::Active);
    ctx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_updates_serialize_without_tearing() {
    let ctx = Context::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let handle = Arc::new(
        ctx.plugin(config_recorder(Arc::clone(&log)), Some(json!({"v": 0})))
            .expect("spawn recorder"),
    );
    handle.join().await.expect("initial activation");

    // Two workers hot-reload the same fiber concurrently. Rejected entries
    // (`fiber_loading`/`fiber_unloading`, the S4 point-in-time guards) are
    // retried; every accepted update must land as one complete activation.
    let mut workers = Vec::new();
    for worker in ["a", "b"] {
        let handle = Arc::clone(&handle);
        workers.push(tokio::spawn(async move {
            for round in 1..=10_i32 {
                let marker = json!({"worker": worker, "v": round});
                let mut attempts = 0_usize;
                loop {
                    match handle.update(marker.clone()).await {
                        Ok(_accepted) => break,
                        Err(err)
                            if err.code == "fiber_loading" || err.code == "fiber_unloading" =>
                        {
                            attempts += 1;
                            assert!(attempts < 1000, "update guards never released");
                            tokio::task::yield_now().await;
                        }
                        Err(err) => panic!("unexpected update failure: {err}"), // contract: only the S4 guard codes are retryable
                    }
                }
            }
        }));
    }
    for worker in workers {
        worker.await.expect("update worker");
    }

    let observed = log.lock().expect("log").clone();
    assert_eq!(observed[0], json!({"v": 0}), "initial activation first");
    let markers: Vec<Value> = (1..=10_i32)
        .flat_map(|round| ["a", "b"].map(move |worker| json!({"worker": worker, "v": round})))
        .collect();
    // No tearing: every activation after the initial one is a complete
    // marker some update submitted (confluence may skip markers — the
    // driver converges on the latest persisted config — but it may never
    // activate a config no update persisted).
    for config in &observed[1..] {
        assert!(
            markers.contains(config),
            "torn or phantom activation: {config}"
        );
    }
    // Per worker the landed markers stay in submission order (a worker
    // submits its next update only after the previous one settled).
    for worker in ["a", "b"] {
        let landed: Vec<i32> = observed[1..]
            .iter()
            .filter(|config| config.get("worker").and_then(Value::as_str) == Some(worker))
            .filter_map(|config| config.get("v").and_then(Value::as_i64))
            .map(|v| i32::try_from(v).expect("small"))
            .collect();
        assert!(
            landed.windows(2).all(|pair| pair[0] < pair[1]),
            "worker {worker} landed out of order: {landed:?}"
        );
    }
    assert!(observed.len() <= 21, "at most one activation per update");
    assert_eq!(handle.state(), FiberState::Active);
    // The kernel keeps taking updates after the storm, and the converged
    // activation matches the last accepted config.
    handle.update(json!({"v": -1})).await.expect("still live");
    assert_eq!(log.lock().expect("log").last(), Some(&json!({"v": -1})));
    ctx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unload_inject_and_event_race_settles_coherently() {
    let ctx = Context::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let consumer = ctx
        .plugin(recording_consumer("svc", Arc::clone(&log)), None)
        .expect("spawn consumer");

    let mut counters: Vec<Arc<AtomicUsize>> = vec![Arc::new(AtomicUsize::new(0))];
    let mut provider = spawn_provider_retried(&ctx, "svc", 0, Arc::clone(&counters[0])).await;
    wait_until(|| consumer.state() == FiberState::Active).await;

    for round in 1..10_i32 {
        counters.push(Arc::new(AtomicUsize::new(0)));
        let old_count = Arc::clone(&counters[usize::try_from(round - 1).expect("small")]);
        let new_count = Arc::clone(&counters[usize::try_from(round).expect("small")]);
        let (old_before, new_before) = (
            old_count.load(AtomicOrdering::SeqCst),
            new_count.load(AtomicOrdering::SeqCst),
        );

        // Three-way race: provider withdrawal, provider injection, and an
        // event dispatch all in flight at once.
        let old = provider.clone();
        let dispose_old = tokio::spawn(async move { old.dispose().await });
        let ctx_new = ctx.clone();
        let new_counter = Arc::clone(&new_count);
        let spawn_new = tokio::spawn(async move {
            spawn_provider_retried(&ctx_new, "svc", round, new_counter).await
        });
        let ctx_dispatch = ctx.clone();
        let dispatch =
            tokio::spawn(async move { ctx_dispatch.parallel("tick", json!(round)).await });

        dispose_old.await.expect("dispose task");
        provider = spawn_new.await.expect("spawn task");
        // A listener unloaded mid-dispatch counts as absent, never failed
        // (S2), so the race can never surface as a dispatch error.
        dispatch
            .await
            .expect("dispatch task")
            .expect("mid-flight unload is listener-absent, not a failure");

        // No duplicate delivery: one dispatch reaches each registered
        // listener at most once, whether it raced the old or the new side.
        let old_delta = old_count.load(AtomicOrdering::SeqCst) - old_before;
        let new_delta = new_count.load(AtomicOrdering::SeqCst) - new_before;
        assert!(
            old_delta <= 1,
            "old listener invoked {old_delta}x in round {round}"
        );
        assert!(
            new_delta <= 1,
            "new listener invoked {new_delta}x in round {round}"
        );

        let expected = usize::try_from(round).expect("small") + 1;
        wait_until(|| {
            consumer.state() == FiberState::Active
                && log.lock().expect("log").len() >= expected
                && log.lock().expect("log").last() == Some(&round)
        })
        .await;
    }

    let entries = log.lock().expect("log").clone();
    assert_eq!(entries.len(), 10, "one activation per provider generation");
    for (round, value) in entries.iter().enumerate() {
        assert_eq!(
            *value,
            i32::try_from(round).expect("small"),
            "activation {round}"
        );
    }
    ctx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn realm_provider_withdrawal_falls_back_to_global_dynamically() {
    let ctx = Context::new();
    let realm = ctx.isolate_realm(ctx.fresh_label());

    let global = ctx
        .plugin(
            plugin("global-provider", vec![], |ctx, _config| {
                Box::pin(async move {
                    ctx.provide("svc", "global".to_string()).await?;
                    Ok(())
                })
            }),
            None,
        )
        .expect("spawn global provider");
    global.join().await.expect("global provider active");

    let log = Arc::new(Mutex::new(Vec::new()));
    let consumer_log = Arc::clone(&log);
    let consumer = realm
        .plugin(
            plugin(
                "realm-consumer",
                vec![Injection::new("svc")],
                move |ctx, _config| {
                    let log = Arc::clone(&consumer_log);
                    Box::pin(async move {
                        let value = ctx.require::<String>("svc")?;
                        log.lock().expect("log").push(value.to_string());
                        Ok(())
                    })
                },
            ),
            None,
        )
        .expect("spawn realm consumer");
    // The realm has no local provider yet: the consumer resolves globally.
    wait_until(|| consumer.state() == FiberState::Active && log.lock().expect("log").len() == 1)
        .await;

    // A realm-local provider arrives: the consumer rewires to it (D15
    // prefers realm-local over the global fallback).
    let local = realm
        .plugin(
            plugin("realm-provider", vec![], |ctx, _config| {
                Box::pin(async move {
                    ctx.provide("svc", "realm".to_string()).await?;
                    Ok(())
                })
            }),
            None,
        )
        .expect("spawn realm provider");
    local.join().await.expect("realm provider active");
    wait_until(|| consumer.state() == FiberState::Active && log.lock().expect("log").len() == 2)
        .await;

    // The realm-local provider withdraws: the consumer must fall back to
    // the global provider dynamically, not park or fail.
    local.dispose().await;
    wait_until(|| consumer.state() == FiberState::Active && log.lock().expect("log").len() == 3)
        .await;

    assert_eq!(
        *log.lock().expect("log"),
        vec![
            "global".to_string(),
            "realm".to_string(),
            "global".to_string()
        ]
    );
    ctx.stop().await;
}

/// A listener that records the invocation and then panics on purpose:
/// fault injection for the per-mode settlement contract (a panic is a
/// contract violation the dispatch layer must survive, never a `Result`).
fn panicking_listener(invoked: Arc<AtomicBool>) -> dougong::Listener {
    Arc::new(move |_ctx, _payload, _next| {
        invoked.store(true, AtomicOrdering::SeqCst);
        panic!("fault-injection listener panic") // contract: the panic IS the injected fault; per-mode settlement is the assertion target
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panicking_listener_settles_per_mode_and_the_kernel_survives() {
    let ctx = Context::new();

    // parallel: the listener runs on its own scoped task, so the panic is
    // contained and aggregated as `listener_panicked`; a healthy listener on
    // the same event is delivered normally.
    let invoked = Arc::new(AtomicBool::new(false));
    let delivered = Arc::new(AtomicBool::new(false));
    ctx.on("panic-parallel", panicking_listener(Arc::clone(&invoked)))
        .await
        .expect("register panicking listener");
    let good = Arc::clone(&delivered);
    ctx.on(
        "panic-parallel",
        Arc::new(move |_ctx, _payload, _next| {
            good.store(true, AtomicOrdering::SeqCst);
            Box::pin(async move { Ok(Value::Null) })
        }),
    )
    .await
    .expect("register good listener");
    let err = ctx
        .parallel("panic-parallel", json!(null))
        .await
        .expect_err("aggregate of the panicking listener");
    assert_eq!(err.code, "aggregate");
    assert!(
        err.to_string().contains("listener task failed"),
        "aggregate carries listener_panicked: {err}"
    );
    assert!(invoked.load(AtomicOrdering::SeqCst));
    assert!(delivered.load(AtomicOrdering::SeqCst));

    // serial/bail/waterfall: plain listeners run inline on the emitter's
    // task, so a panic crosses the dispatch await (TS-like propagation);
    // the dispatch task dies, and the kernel must be unaffected. Each arm
    // dispatches from a sacrificial task and asserts the join failure.
    for (index, mode) in ["serial", "bail", "waterfall"].iter().enumerate() {
        let event = format!("panic-{mode}");
        let invoked = Arc::new(AtomicBool::new(false));
        ctx.on(&event, panicking_listener(Arc::clone(&invoked)))
            .await
            .expect("register panicking listener");
        let dispatch_ctx = ctx.clone();
        let dispatch = tokio::spawn(async move {
            match index {
                0 => dispatch_ctx.serial(&event, json!(null)).await,
                1 => dispatch_ctx.bail(&event, json!(null)).await,
                _ => {
                    dispatch_ctx
                        .waterfall(&event, json!(null), |payload| {
                            Box::pin(async move { Ok(payload) })
                        })
                        .await
                }
            }
        });
        let join_error = dispatch
            .await
            .expect_err("the panic crosses the inline dispatch await");
        assert!(join_error.is_panic(), "{mode} dispatch died of the panic");
        assert!(invoked.load(AtomicOrdering::SeqCst), "{mode} listener ran");
    }

    // emit: fire-and-forget runs each listener on its own scoped task; the
    // panicking task dies silently (S5-recorded contract) while the healthy
    // listener on the same event is still delivered.
    let invoked = Arc::new(AtomicBool::new(false));
    let delivered = Arc::new(AtomicBool::new(false));
    ctx.on("panic-emit", panicking_listener(Arc::clone(&invoked)))
        .await
        .expect("register panicking listener");
    let good = Arc::clone(&delivered);
    ctx.on(
        "panic-emit",
        Arc::new(move |_ctx, _payload, _next| {
            good.store(true, AtomicOrdering::SeqCst);
            Box::pin(async move { Ok(Value::Null) })
        }),
    )
    .await
    .expect("register good listener");
    ctx.emit("panic-emit", json!(null));
    wait_until(|| invoked.load(AtomicOrdering::SeqCst) && delivered.load(AtomicOrdering::SeqCst))
        .await;

    // The kernel survived every arm: a normal dispatch still settles.
    let reply = ctx
        .parallel("panic-parallel", json!("again"))
        .await
        .expect_err("still only the panicking listener fails");
    assert_eq!(reply.code, "aggregate");
    ctx.stop().await;
}
