// Rust guideline compliant 2026-10-24
//! Contention benchmarks for the shared root-state structures (performance
//! track P1, improve requirement 一.1): notify fan-out storms (reflect store,
//! dependency index, fiber table), resolve storms (store read path), and
//! spawn/dispose storms (fiber table, interest index).
//!
//! Ignored by default so the regular gates stay hermetic; run on demand:
//!
//! ```sh
//! cargo test --release -p dougong --test bench_contention -- --ignored --nocapture
//! ```
//!
//! Every scenario prints one machine-readable `BENCH name=... ops=...
//! wall_ms=...` line so the sharding change is compared against the recorded
//! pre-change baseline on the same machine. These harnesses assert
//! *correctness anchors* (resolved values, convergence) — a broken kernel
//! must not be able to produce "fast" numbers — but never absolute timings.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use dougong::{Context, FiberHandle, Injection, LogLevel, Plugin, plugin};

/// Distinct services exercised per scenario: wide enough that a sharded
/// store spreads over shards, small enough for fast setup.
const SERVICES: usize = 64;
/// Consumer fibers per service in the fan-out storm (the notify fan-out).
const CONSUMERS_PER_SERVICE: usize = 8;
/// Concurrent writer/reader/spawner tasks per scenario.
const TASKS: usize = 16;
/// Provider replacements per writer task in the fan-out storm.
const CHURN_ROUNDS: usize = 100;
/// Service reads per task in the resolve storm.
const READS_PER_TASK: usize = 200_000;
/// Spawn+dispose cycles per task in the registry storm.
const SPAWN_CYCLES: usize = 10_000;
/// Hard ceiling for one scenario: a hang fails the bench instead of
/// stalling a manual run.
const RAIL: Duration = Duration::from_secs(180);

/// Names of the `SERVICES` services under one scenario prefix.
fn service_names(prefix: &str) -> Vec<String> {
    (0..SERVICES)
        .map(|index| format!("{prefix}.{index}"))
        .collect()
}

/// A plugin providing `service` with a constant value (1).
fn provider_plugin(service: &str) -> Arc<dyn Plugin> {
    let service = service.to_string();
    plugin("bench.provider", vec![], move |ctx, _config| {
        let service = service.clone();
        Box::pin(async move {
            ctx.provide(&service, 1_u64).await?;
            Ok(())
        })
    })
}

/// Spawns one provider of the shared plugin, retrying while the slot is
/// still held by the outgoing provider (`duplicate_provide` means the
/// withdrawal has not landed yet; any other startup error is a failure).
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

/// Emits one machine-readable `BENCH` result line through the kernel logger
/// (the repo's only sanctioned output channel); greppable in `--nocapture`
/// output.
fn report(ctx: &Context, name: &str, ops: usize, wall: Duration) {
    ctx.logger().log(
        LogLevel::Info,
        "bench",
        &format!("BENCH {name} ops={ops} wall_ms={}", wall.as_millis()),
    );
}

/// B1: notify fan-out storm. `TASKS` writer tasks each own a disjoint slice
/// of the services and repeatedly replace the provider (dispose + respawn),
/// forcing withdrawal notify + drain + re-provide notify across
/// `CONSUMERS_PER_SERVICE` consumers per service. Contends on the reflect
/// store, the dependency index (interest candidates + edge commits), and
/// the fiber table (candidate upgrades in `notify`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run on demand with --ignored"]
async fn b1_notify_fanout_storm() {
    let ctx = Context::new();
    let names = service_names("b1.svc");

    // Consumers: 8 per service, parked until the first provider lands.
    let mut consumers = Vec::new();
    for name in &names {
        for _ in 0..CONSUMERS_PER_SERVICE {
            let consumer = plugin(
                "bench.consumer",
                vec![Injection::new(name.clone())],
                |_ctx, _config| Box::pin(async move { Ok(()) }),
            );
            consumers.push(ctx.plugin(consumer, None).expect("spawn consumer"));
        }
    }

    // Per-task work slices (interleaved assignment spreads a task's services
    // over different shards under the P1 layout).
    let mut slices: Vec<Vec<(String, Arc<dyn Plugin>)>> = (0..TASKS).map(|_| Vec::new()).collect();
    for (index, name) in names.iter().enumerate() {
        slices[index % TASKS].push((name.clone(), provider_plugin(name)));
    }
    let mut handle_slices: Vec<Vec<FiberHandle>> = Vec::new();
    for slice in &slices {
        let mut handles = Vec::new();
        for (_name, provider) in slice {
            handles.push(spawn_provider_retried(&ctx, provider).await);
        }
        handle_slices.push(handles);
    }
    // Setup correctness anchor: every consumer converged Active.
    for consumer in &consumers {
        consumer.join().await.expect("consumer active after setup");
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(TASKS + 1));
    let mut tasks = Vec::new();
    for slice in slices.into_iter().zip(handle_slices) {
        let ctx = ctx.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            let (slice, mut handles) = slice;
            barrier.wait().await;
            for _round in 0..CHURN_ROUNDS {
                for (handle, (_name, provider)) in handles.iter_mut().zip(&slice) {
                    handle.dispose().await;
                    *handle = spawn_provider_retried(&ctx, provider).await;
                }
            }
            handles
        }));
    }
    barrier.wait().await;
    let start = Instant::now();
    let mut final_handles = Vec::new();
    for task in tasks {
        final_handles.push(
            tokio::time::timeout(RAIL, task)
                .await
                .expect("bench stalled"),
        );
    }
    let wall = start.elapsed();
    let ops = TASKS * CHURN_ROUNDS * (SERVICES / TASKS) * 2;
    report(&ctx, "b1_notify_fanout_storm", ops, wall);

    // Correctness anchor: consumers ride every replacement and settle Active.
    for consumer in &consumers {
        consumer.join().await.expect("consumer active after storm");
    }
    ctx.stop().await;
    drop(final_handles);
}

/// B2: resolve storm. `TASKS` reader tasks hammer `Context::get` for
/// `SERVICES` distinct live services through the root store path (no
/// committed-view shortcut: the names are not declared dependencies).
/// Contends on the reflect store read path plus the provider-liveness set.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run on demand with --ignored"]
async fn b2_resolve_storm() {
    let ctx = Context::new();
    let names = service_names("b2.svc");
    for name in &names {
        let provider = provider_plugin(name);
        let handle = ctx.plugin(provider, None).expect("spawn provider");
        handle.join().await.expect("provider active");
    }

    let checksum = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(TASKS + 1));
    let mut tasks = Vec::new();
    for _ in 0..TASKS {
        let ctx = ctx.clone();
        let names = names.clone();
        let checksum = Arc::clone(&checksum);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut local = 0_u64;
            for round in 0..READS_PER_TASK {
                let name = &names[round % SERVICES];
                let value = ctx.get::<u64>(name).expect("service resolves");
                local = local.wrapping_add(*value);
            }
            checksum.fetch_add(local, AtomicOrdering::Relaxed);
        }));
    }
    barrier.wait().await;
    let start = Instant::now();
    for task in tasks {
        tokio::time::timeout(RAIL, task)
            .await
            .expect("bench stalled")
            .expect("reader task");
    }
    let wall = start.elapsed();
    let ops = TASKS * READS_PER_TASK;
    report(&ctx, "b2_resolve_storm", ops, wall);

    // Every read resolved the constant value 1.
    let expected = u64::try_from(ops).expect("op count fits u64");
    assert_eq!(checksum.load(AtomicOrdering::Relaxed), expected);
    ctx.stop().await;
}

/// B3: spawn/dispose storm. `TASKS` tasks repeatedly start and dispose one
/// shared plugin (one registry record; per-cycle fiber-table insert/remove,
/// interest-index add/remove, active-set flip, and dependency-edge
/// commit/uncommit against the anchor service).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run on demand with --ignored"]
async fn b3_spawn_dispose_storm() {
    let ctx = Context::new();
    let anchor = provider_plugin("b3.anchor");
    let anchor = ctx.plugin(anchor, None).expect("spawn anchor");
    anchor.join().await.expect("anchor active");
    let worker = plugin(
        "bench.worker",
        vec![Injection::new("b3.anchor")],
        |_ctx, _config| Box::pin(async move { Ok(()) }),
    );

    let barrier = Arc::new(tokio::sync::Barrier::new(TASKS + 1));
    let mut tasks = Vec::new();
    for _ in 0..TASKS {
        let ctx = ctx.clone();
        let worker = Arc::clone(&worker);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            for _cycle in 0..SPAWN_CYCLES {
                let handle = ctx.plugin(Arc::clone(&worker), None).expect("spawn worker");
                handle.join().await.expect("worker active");
                handle.dispose().await;
            }
        }));
    }
    barrier.wait().await;
    let start = Instant::now();
    for task in tasks {
        tokio::time::timeout(RAIL, task)
            .await
            .expect("bench stalled")
            .expect("spawner task");
    }
    let wall = start.elapsed();
    let ops = TASKS * SPAWN_CYCLES * 2;
    report(&ctx, "b3_spawn_dispose_storm", ops, wall);

    // Only the root fiber and the anchor provider remain.
    assert_eq!(ctx.live_fibers(), 2);
    ctx.stop().await;
}
