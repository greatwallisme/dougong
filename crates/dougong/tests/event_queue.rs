// Rust guideline compliant 2026-10-24
//! Behavioral coverage for bounded event dispatch (P2).

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use dougong::{
    Context, ContextOptions, EmitOutcome, EventPolicy, EventQueueConfig, Listener, OverflowPolicy,
    plugin,
};
use serde_json::Value;
use tokio::sync::watch;

const RAIL: Duration = Duration::from_secs(5);

const fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("test capacities are non-zero")
}

fn options(
    workers: usize,
    capacity: usize,
    policies: HashMap<String, EventPolicy>,
) -> ContextOptions {
    ContextOptions {
        events: EventQueueConfig {
            workers: nonzero(workers),
            queue_capacity: Some(nonzero(capacity)),
            policies,
        },
        ..ContextOptions::default()
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(RAIL, async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition did not settle before the test rail");
}

fn gated_listener(
    started: Arc<AtomicUsize>,
    delivered: Arc<AtomicUsize>,
    release: watch::Receiver<bool>,
) -> Listener {
    Arc::new(move |_ctx, _payload, _next| {
        let started = Arc::clone(&started);
        let delivered = Arc::clone(&delivered);
        let mut release = release.clone();
        Box::pin(async move {
            started.fetch_add(1, Ordering::Relaxed);
            while !*release.borrow() {
                release.changed().await.expect("release sender stays alive");
            }
            delivered.fetch_add(1, Ordering::Relaxed);
            Ok(Value::Null)
        })
    })
}

#[tokio::test]
async fn fixed_workers_bound_concurrency_without_losing_delivery() {
    let ctx = Context::new_with_options(options(2, 16, HashMap::new()));
    let started = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = watch::channel(false);
    for _ in 0..4 {
        let started = Arc::clone(&started);
        let delivered = Arc::clone(&delivered);
        let active = Arc::clone(&active);
        let peak = Arc::clone(&peak);
        let release = release_rx.clone();
        let listener: Listener = Arc::new(move |_ctx, _payload, _next| {
            let started = Arc::clone(&started);
            let delivered = Arc::clone(&delivered);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let mut release = release.clone();
            Box::pin(async move {
                started.fetch_add(1, Ordering::Relaxed);
                let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                peak.fetch_max(current, Ordering::Relaxed);
                while !*release.borrow() {
                    release.changed().await.expect("release sender stays alive");
                }
                active.fetch_sub(1, Ordering::Relaxed);
                delivered.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Null)
            })
        });
        ctx.on("bounded", listener)
            .await
            .expect("register listener");
    }

    ctx.emit("bounded", Value::Null);
    wait_until(|| started.load(Ordering::Relaxed) == 2).await;
    assert_eq!(delivered.load(Ordering::Relaxed), 0);
    release_tx.send(true).expect("release listeners");
    wait_until(|| delivered.load(Ordering::Relaxed) == 4).await;
    assert_eq!(peak.load(Ordering::Relaxed), 2);
    ctx.stop().await;
}

#[tokio::test]
async fn per_event_cap_does_not_block_other_event_names() {
    let mut policies = HashMap::new();
    policies.insert(
        "hot".to_string(),
        EventPolicy {
            max_concurrency: Some(nonzero(1)),
            ..EventPolicy::default()
        },
    );
    let ctx = Context::new_with_options(options(2, 16, policies));
    let hot_started = Arc::new(AtomicUsize::new(0));
    let hot_delivered = Arc::new(AtomicUsize::new(0));
    let other_delivered = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = watch::channel(false);
    ctx.on(
        "hot",
        gated_listener(
            Arc::clone(&hot_started),
            Arc::clone(&hot_delivered),
            release_rx,
        ),
    )
    .await
    .expect("register hot listener");
    let other = Arc::clone(&other_delivered);
    ctx.on(
        "other",
        Arc::new(move |_ctx, _payload, _next| {
            let other = Arc::clone(&other);
            Box::pin(async move {
                other.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Null)
            })
        }),
    )
    .await
    .expect("register independent listener");

    ctx.emit("hot", Value::Null);
    wait_until(|| hot_started.load(Ordering::Relaxed) == 1).await;
    ctx.emit("hot", Value::Null);
    ctx.emit("other", Value::Null);
    wait_until(|| other_delivered.load(Ordering::Relaxed) == 1).await;
    assert_eq!(hot_delivered.load(Ordering::Relaxed), 0);
    release_tx.send(true).expect("release hot listener");
    wait_until(|| hot_delivered.load(Ordering::Relaxed) == 2).await;
    ctx.stop().await;
}

#[tokio::test]
async fn bounded_backpressure_waits_for_capacity_without_dropping() {
    let ctx = Context::new_with_options(options(1, 1, HashMap::new()));
    let started = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = watch::channel(false);
    ctx.on(
        "backpressure",
        gated_listener(Arc::clone(&started), Arc::clone(&delivered), release_rx),
    )
    .await
    .expect("register gated listener");

    assert_eq!(
        ctx.emit_with_policy("backpressure", Value::Null).await,
        EmitOutcome::Queued
    );
    wait_until(|| started.load(Ordering::Relaxed) == 1).await;
    assert_eq!(
        ctx.emit_with_policy("backpressure", Value::Null).await,
        EmitOutcome::Queued
    );
    let mut third = {
        let ctx = ctx.clone();
        tokio::spawn(async move { ctx.emit_with_policy("backpressure", Value::Null).await })
    };
    tokio::time::timeout(Duration::from_millis(50), &mut third)
        .await
        .expect_err("a full bounded queue applies backpressure");
    release_tx.send(true).expect("release listeners");
    let third = tokio::time::timeout(RAIL, &mut third)
        .await
        .expect("backpressured sender resumes")
        .expect("backpressured task joins");
    assert_eq!(third, EmitOutcome::Queued);
    wait_until(|| delivered.load(Ordering::Relaxed) == 3).await;
    ctx.stop().await;
}

#[tokio::test]
async fn drop_policy_discards_only_when_the_bounded_queue_is_full() {
    let mut policies = HashMap::new();
    policies.insert(
        "noise".to_string(),
        EventPolicy {
            overflow: OverflowPolicy::Drop,
            ..EventPolicy::default()
        },
    );
    let ctx = Context::new_with_options(options(1, 1, policies));
    let started = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = watch::channel(false);
    ctx.on(
        "noise",
        gated_listener(Arc::clone(&started), Arc::clone(&delivered), release_rx),
    )
    .await
    .expect("register gated listener");

    assert_eq!(
        ctx.emit_with_policy("noise", Value::Null).await,
        EmitOutcome::Queued
    );
    wait_until(|| started.load(Ordering::Relaxed) == 1).await;
    assert_eq!(
        ctx.emit_with_policy("noise", Value::Null).await,
        EmitOutcome::Queued
    );
    ctx.emit("noise", Value::Null);
    assert_eq!(
        ctx.emit_with_policy("noise", Value::Null).await,
        EmitOutcome::Dropped
    );
    release_tx.send(true).expect("release listeners");
    wait_until(|| delivered.load(Ordering::Relaxed) == 2).await;
    ctx.stop().await;
}

#[tokio::test]
async fn listener_panic_does_not_kill_the_worker_pool() {
    let ctx = Context::new_with_options(options(1, 16, HashMap::new()));
    let delivered = Arc::new(AtomicUsize::new(0));
    ctx.on(
        "panic",
        Arc::new(|_ctx, _payload, _next| {
            Box::pin(async { panic!("deliberate listener panic") }) // panic injection contract
        }),
    )
    .await
    .expect("register panicking listener");
    let good = Arc::clone(&delivered);
    ctx.on(
        "panic",
        Arc::new(move |_ctx, _payload, _next| {
            let good = Arc::clone(&good);
            Box::pin(async move {
                good.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Null)
            })
        }),
    )
    .await
    .expect("register healthy listener");
    ctx.emit("panic", Value::Null);
    wait_until(|| delivered.load(Ordering::Relaxed) == 1).await;

    let other = Arc::clone(&delivered);
    ctx.on(
        "after-panic",
        Arc::new(move |_ctx, _payload, _next| {
            let other = Arc::clone(&other);
            Box::pin(async move {
                other.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Null)
            })
        }),
    )
    .await
    .expect("register follow-up listener");
    ctx.emit("after-panic", Value::Null);
    wait_until(|| delivered.load(Ordering::Relaxed) == 2).await;
    ctx.stop().await;
}

#[tokio::test]
async fn unloading_a_listener_before_dequeue_skips_its_invocation() {
    let ctx = Context::new_with_options(options(1, 16, HashMap::new()));
    let blocker_started = Arc::new(AtomicUsize::new(0));
    let blocker_delivered = Arc::new(AtomicUsize::new(0));
    let victim_calls = Arc::new(AtomicUsize::new(0));
    let sentinel_calls = Arc::new(AtomicUsize::new(0));
    let (release_tx, release_rx) = watch::channel(false);
    ctx.on(
        "block",
        gated_listener(
            Arc::clone(&blocker_started),
            Arc::clone(&blocker_delivered),
            release_rx,
        ),
    )
    .await
    .expect("register blocking listener");
    let victim_probe = Arc::clone(&victim_calls);
    let victim = plugin("victim", vec![], move |ctx, _config| {
        let victim_calls = Arc::clone(&victim_probe);
        Box::pin(async move {
            let listener: Listener = Arc::new(move |_ctx, _payload, _next| {
                let victim_calls = Arc::clone(&victim_calls);
                Box::pin(async move {
                    victim_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(Value::Null)
                })
            });
            ctx.on("victim", listener).await?;
            Ok(())
        })
    });
    let victim = ctx.plugin(victim, None).expect("spawn victim listener");
    victim.join().await.expect("victim listener active");
    let sentinel = Arc::clone(&sentinel_calls);
    ctx.on(
        "sentinel",
        Arc::new(move |_ctx, _payload, _next| {
            let sentinel = Arc::clone(&sentinel);
            Box::pin(async move {
                sentinel.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Null)
            })
        }),
    )
    .await
    .expect("register sentinel listener");

    ctx.emit("block", Value::Null);
    wait_until(|| blocker_started.load(Ordering::Relaxed) == 1).await;
    ctx.emit("victim", Value::Null);
    victim.dispose().await;
    ctx.emit("sentinel", Value::Null);
    release_tx.send(true).expect("release blocking listener");
    wait_until(|| sentinel_calls.load(Ordering::Relaxed) == 1).await;
    assert_eq!(victim_calls.load(Ordering::Relaxed), 0);
    assert_eq!(blocker_delivered.load(Ordering::Relaxed), 1);
    ctx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_producers_preserve_delivery_and_per_event_caps() {
    const NAMES: [&str; 4] = ["storm.a", "storm.b", "storm.c", "storm.d"];
    const LISTENERS: usize = 4;
    const PRODUCERS: usize = 8;
    const ROUNDS: usize = 500;
    let mut policies = HashMap::new();
    for name in NAMES {
        policies.insert(
            name.to_string(),
            EventPolicy {
                max_concurrency: Some(nonzero(2)),
                ..EventPolicy::default()
            },
        );
    }
    let ctx = Context::new_with_options(options(4, 64, policies));
    let delivered = Arc::new(AtomicUsize::new(0));
    let active: [Arc<AtomicUsize>; 4] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let peaks: [Arc<AtomicUsize>; 4] = std::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    for (index, name) in NAMES.into_iter().enumerate() {
        for _ in 0..LISTENERS {
            let delivered = Arc::clone(&delivered);
            let active = Arc::clone(&active[index]);
            let peak = Arc::clone(&peaks[index]);
            let listener: Listener = Arc::new(move |_ctx, _payload, _next| {
                let delivered = Arc::clone(&delivered);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                Box::pin(async move {
                    let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                    peak.fetch_max(current, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                    active.fetch_sub(1, Ordering::Relaxed);
                    delivered.fetch_add(1, Ordering::Relaxed);
                    Ok(Value::Null)
                })
            });
            ctx.on(name, listener)
                .await
                .expect("register storm listener");
        }
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(PRODUCERS + 1));
    let mut producers = Vec::new();
    for producer in 0..PRODUCERS {
        let ctx = ctx.clone();
        let barrier = Arc::clone(&barrier);
        producers.push(tokio::spawn(async move {
            barrier.wait().await;
            for round in 0..ROUNDS {
                ctx.emit(NAMES[(producer + round) % NAMES.len()], Value::Null);
                if round % 32 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }
    barrier.wait().await;
    for producer in producers {
        tokio::time::timeout(RAIL, producer)
            .await
            .expect("producer stalled")
            .expect("producer task joined");
    }
    wait_until(|| delivered.load(Ordering::Relaxed) == PRODUCERS * ROUNDS * LISTENERS).await;
    for peak in peaks {
        assert!(peak.load(Ordering::Relaxed) <= 2);
    }
    ctx.stop().await;
}
