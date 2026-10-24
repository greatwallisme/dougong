// Rust guideline compliant 2026-10-24
//! Deterministic ordering of dependency updates (stability track S6, design
//! decision D21): concurrent provider changes to the same service slot run
//! as serialized sections, consumers always converge on the exact fixpoint,
//! and `internal/service` events are generated in commit order.
//!
//! The multi-threaded storms assert *exact* final states per round (any lost
//! or torn update tears them); the observation-order pin runs on the
//! current-thread runtime, where spawned listener tasks poll in spawn order.

use std::sync::atomic::{AtomicI32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, FiberHandle, FiberState, Injection, plugin};
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

/// A provider of `service` carrying the given value marker.
fn provider_plugin(
    name: &'static str,
    service: &'static str,
    value: i32,
) -> Arc<dyn dougong::Plugin> {
    plugin(name, vec![], move |ctx, _config| {
        Box::pin(async move {
            ctx.provide(service, value).await?;
            Ok(())
        })
    })
}

/// Spawns a provider, retrying while the slot is still held by the outgoing
/// provider (`duplicate_provide` means the withdrawal has not landed yet;
/// any other startup error is a real failure).
async fn spawn_provider_retried(ctx: &Context, service: &'static str, value: i32) -> FiberHandle {
    let attempts = AtomicI32::new(0);
    loop {
        let handle = ctx
            .plugin(provider_plugin("provider", service, value), None)
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

/// A consumer recording every activation's resolved values.
fn recording_consumer(
    deps: Vec<&'static str>,
    log: Arc<Mutex<Vec<i32>>>,
) -> Arc<dyn dougong::Plugin> {
    let injections = deps.iter().map(|name| Injection::new(*name)).collect();
    plugin("consumer", injections, move |ctx, _config| {
        let deps = deps.clone();
        let log = Arc::clone(&log);
        Box::pin(async move {
            for name in deps {
                let value = ctx.require::<i32>(name)?;
                log.lock().expect("log").push(*value);
            }
            Ok(())
        })
    })
}

/// Replaces the provider of `service` by racing the old provider's disposal
/// against the new provider's spawn; returns once the consumer has visibly
/// converged on `value`.
async fn replace_provider(
    ctx: &Context,
    consumer: &FiberHandle,
    old: FiberHandle,
    service: &'static str,
    value: i32,
    log: &Arc<Mutex<Vec<i32>>>,
    activations: usize,
) -> FiberHandle {
    let dispose_old = tokio::spawn(async move { old.dispose().await });
    let ctx_new = ctx.clone();
    let spawn_new =
        tokio::spawn(async move { spawn_provider_retried(&ctx_new, service, value).await });
    dispose_old.await.expect("dispose task");
    let new = spawn_new.await.expect("spawn task");
    wait_until(|| {
        consumer.state() == FiberState::Active
            && log.lock().expect("log").len() >= activations
            && log.lock().expect("log").last() == Some(&value)
    })
    .await;
    new
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_slot_replacement_converges_deterministically() {
    let ctx = Context::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let consumer = ctx
        .plugin(recording_consumer(vec!["svc"], Arc::clone(&log)), None)
        .expect("spawn consumer");

    let mut provider = spawn_provider_retried(&ctx, "svc", 0).await;
    wait_until(|| consumer.state() == FiberState::Active).await;

    // Round N replaces the provider under a genuine dispose/spawn race; the
    // consumer must land on the round's marker every single time.
    for round in 1..30_i32 {
        let expected_activations = usize::try_from(round).expect("round fits") + 1;
        provider = replace_provider(
            &ctx,
            &consumer,
            provider,
            "svc",
            round,
            &log,
            expected_activations,
        )
        .await;
    }

    let entries = log.lock().expect("log").clone();
    assert_eq!(entries.len(), 30, "one activation per provider generation");
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
async fn multi_service_storm_fixpoint_is_exact() {
    let services: [&'static str; 4] = ["s0", "s1", "s2", "s3"];
    let logs: Vec<Arc<Mutex<Vec<i32>>>> =
        (0..3).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
    let deps: [Vec<&'static str>; 3] = [
        vec!["s0", "s1"],
        vec!["s2", "s3"],
        vec!["s0", "s1", "s2", "s3"],
    ];
    let dep_widths = [2_usize, 2, 4];

    let ctx = Context::new();
    let mut consumers = Vec::new();
    for (index, deps) in deps.iter().enumerate() {
        consumers.push(
            ctx.plugin(
                recording_consumer(deps.clone(), Arc::clone(&logs[index])),
                None,
            )
            .expect("spawn consumer"),
        );
    }
    let mut providers: Vec<FiberHandle> = Vec::new();
    for service in services {
        providers.push(spawn_provider_retried(&ctx, service, 0).await);
    }
    for consumer in &consumers {
        wait_until(|| consumer.state() == FiberState::Active).await;
    }

    for round in 1..10_i32 {
        // Replace every provider at once: four dispose/spawn pairs racing.
        let mut tasks = Vec::new();
        for (slot, service) in services.into_iter().enumerate() {
            let old = providers[slot].clone();
            let ctx = ctx.clone();
            tasks.push(tokio::spawn(async move {
                old.dispose().await;
                spawn_provider_retried(&ctx, service, round).await
            }));
        }
        for (slot, task) in tasks.into_iter().enumerate() {
            providers[slot] = task.await.expect("replacement task");
        }
        // Every consumer must converge on exactly the round's marker in
        // every dependency slot — the fixpoint is a pure function of round.
        for (index, consumer) in consumers.iter().enumerate() {
            let log = Arc::clone(&logs[index]);
            let width = dep_widths[index];
            wait_until(|| {
                consumer.state() == FiberState::Active && {
                    let entries = log.lock().expect("log");
                    let tail = &entries[entries.len().saturating_sub(width)..];
                    entries.len() >= width && tail.iter().all(|value| *value == round)
                }
            })
            .await;
        }
    }
    ctx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_fiber_two_service_refresh_stays_coherent() {
    let ctx = Context::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let consumer = ctx
        .plugin(
            recording_consumer(vec!["svc-a", "svc-b"], Arc::clone(&log)),
            None,
        )
        .expect("spawn consumer");
    let provider_a = spawn_provider_retried(&ctx, "svc-a", 0).await;
    let provider_b = spawn_provider_retried(&ctx, "svc-b", 0).await;
    wait_until(|| consumer.state() == FiberState::Active).await;

    // Replace both dependencies concurrently: the shared consumer's
    // refreshes merge through its single driver, and the final epoch must
    // carry both new providers — never one new plus one stale.
    let mut a = provider_a;
    let mut b = provider_b;
    for round in 1..20_i32 {
        let old_a = a.clone();
        let old_b = b.clone();
        let ctx_a = ctx.clone();
        let ctx_b = ctx.clone();
        let (ta, tb) = (
            tokio::spawn(async move {
                old_a.dispose().await;
                spawn_provider_retried(&ctx_a, "svc-a", round).await
            }),
            tokio::spawn(async move {
                old_b.dispose().await;
                spawn_provider_retried(&ctx_b, "svc-b", round + 100).await
            }),
        );
        a = ta.await.expect("replace a");
        b = tb.await.expect("replace b");
        wait_until(|| {
            consumer.state() == FiberState::Active && {
                let entries = log.lock().expect("log");
                let tail = &entries[entries.len().saturating_sub(2)..];
                entries.len() >= 2 && tail == [round, round + 100].as_slice()
            }
        })
        .await;
    }
    ctx.stop().await;
}

#[tokio::test]
async fn service_events_generate_in_commit_order() {
    // Current-thread runtime: spawned listener tasks poll in spawn order, so
    // the captured sequence equals the generation sequence.
    let ctx = Context::new();
    let stream = Arc::new(Mutex::new(Vec::new()));
    let stream_listener = Arc::clone(&stream);
    ctx.on(
        "internal/service",
        Arc::new(move |_ctx, payload, _next| {
            let stream = Arc::clone(&stream_listener);
            Box::pin(async move {
                if payload.get("service").and_then(Value::as_str) == Some("svc") {
                    let provided = payload
                        .get("provided")
                        .and_then(Value::as_bool)
                        .expect("provided flag");
                    stream.lock().expect("stream").push(provided);
                }
                Ok(Value::Null)
            })
        }),
    )
    .await
    .expect("listen");

    // Each provider generation announces itself on activation; each disposal
    // announces the withdrawal. Commit order is strictly alternating.
    for value in 0..3 {
        let provider = spawn_provider_retried(&ctx, "svc", value).await;
        provider.join().await.expect("provider active");
        provider.dispose().await;
    }

    wait_until(|| stream.lock().expect("stream").len() >= 6).await;
    // Fire-and-forget delivery can lag the disposal return; the grace window
    // only stabilizes the capture, never the order.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        *stream.lock().expect("stream"),
        [true, false, true, false, true, false]
    );
    ctx.stop().await;
}
