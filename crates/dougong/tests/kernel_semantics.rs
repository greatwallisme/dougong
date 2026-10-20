// Rust guideline compliant 2026-10-20
//! Kernel semantics tests: effects, events, isolation, lifecycle.
//!
//! Maps the jincdream upstream-compatibility surface (dispose/events/fiber/
//! isolate/service/plugin suites) onto dougong's API.

use std::sync::Arc;

use dougong::{Context, FiberState, Injection, plugin};
use serde_json::{Value, json};
use std::sync::Mutex;

#[tokio::test]
async fn effects_run_lifo_and_disposal_is_idempotent() {
    let ctx = Context::new();
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    let log_in_plugin = Arc::clone(&log);
    let plugin = plugin("effects", vec![], move |ctx, _config| {
        let log = Arc::clone(&log_in_plugin);
        Box::pin(async move {
            for label in ["first", "second"] {
                let log_for_effect = Arc::clone(&log);
                let disposer: dougong::Disposer = Box::new(move || {
                    let log = Arc::clone(&log_for_effect);
                    Box::pin(async move {
                        log.lock().unwrap().push(label);
                    })
                });
                ctx.effect(label, async move { Ok(Some(disposer)) }).await?;
            }
            Ok(())
        })
    });

    let fiber = ctx.plugin(plugin, None).expect("spawn");
    fiber.join().await.expect("active");
    assert_eq!(fiber.effect_count(), 2);
    fiber.dispose().await;
    // LIFO: second was registered last, disposes first.
    assert_eq!(*log.lock().unwrap(), vec!["second", "first"]);
}

#[tokio::test]
async fn failed_activation_rolls_back_effects_transactionally() {
    let ctx = Context::new();
    let rolled_back: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    let rolled_back_in_plugin = Arc::clone(&rolled_back);
    let plugin = plugin("boom", vec![], move |ctx, _config| {
        let rolled_back_for_body = Arc::clone(&rolled_back_in_plugin);
        Box::pin(async move {
            let flag = Arc::clone(&rolled_back_for_body);
            let disposer: dougong::Disposer = Box::new(move || {
                let flag = Arc::clone(&flag);
                Box::pin(async move {
                    *flag.lock().unwrap() = true;
                })
            });
            ctx.effect("doomed", async move { Ok(Some(disposer)) })
                .await?;
            Err(dougong::Error::msg("startup exploded"))
        })
    });

    let fiber = ctx.plugin(plugin, None).expect("spawn");
    assert!(fiber.join().await.is_err(), "startup error surfaces");
    assert_eq!(fiber.state(), FiberState::Failed);
    assert!(*rolled_back.lock().unwrap(), "collected effect rolled back");
    assert_eq!(fiber.effect_count(), 0, "no effects survive a failed body");
}

#[tokio::test]
async fn event_dispatch_modes_match_cordis_semantics() {
    let ctx = Context::new();

    // serial stops at first bail; non-null values bail.
    let hits: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
    for id in [1, 2, 3] {
        let hits = Arc::clone(&hits);
        ctx.on(
            "evt/serial",
            Arc::new(move |_ctx, _payload, _next| {
                let hits = Arc::clone(&hits);
                Box::pin(async move {
                    hits.lock().unwrap().push(id);
                    if id == 2 {
                        Ok(json!({ "bail": id }))
                    } else {
                        Ok(Value::Null)
                    }
                })
            }),
        )
        .await
        .expect("register");
    }
    let result = ctx
        .serial("evt/serial", json!(null))
        .await
        .expect("dispatch");
    assert_eq!(result, json!({ "bail": 2 }));
    assert_eq!(
        *hits.lock().unwrap(),
        vec![1, 2],
        "third listener not reached"
    );

    // waterfall composes around `next`; skipping `next` vetoes the fallback.
    let ctx2 = ctx.extend();
    let vetoed = ctx2
        .waterfall("evt/waterfall", json!(1), |_payload| {
            Box::pin(async { Ok(json!("fallback-ran")) })
        })
        .await;
    // No listeners: fallback runs.
    let _composed = vetoed.expect("waterfall fallback runs");

    ctx.on(
        "evt/waterfall",
        Arc::new(|_ctx, payload, _next| Box::pin(async move { Ok(json!({ "doubled": payload })) })),
    )
    .await
    .expect("register");
    let composed = ctx
        .waterfall("evt/waterfall", json!(1), |_| {
            Box::pin(async { Ok(json!("builtin")) })
        })
        .await
        .expect("waterfall");
    assert_eq!(composed, json!({ "doubled": 1 }));

    // parallel aggregates failures.
    ctx.on(
        "evt/parallel",
        Arc::new(|_ctx, _p, _n| Box::pin(async { Err(dougong::Error::msg("listener exploded")) })),
    )
    .await
    .expect("register");
    let parallel = ctx.parallel("evt/parallel", json!(null)).await;
    assert!(parallel.is_err(), "failures aggregate");

    // once disposes itself after the first call.
    let counter: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
    let counter2 = Arc::clone(&counter);
    ctx.once(
        "evt/once",
        Arc::new(move |_ctx, _p, _n| {
            let counter = Arc::clone(&counter2);
            Box::pin(async move {
                *counter.lock().unwrap() += 1;
                Ok(Value::Null)
            })
        }),
    )
    .await
    .expect("register");
    let _ = ctx.serial("evt/once", json!(null)).await;
    let _ = ctx.serial("evt/once", json!(null)).await;
    assert_eq!(*counter.lock().unwrap(), 1);
}

#[tokio::test]
async fn isolation_routes_service_resolution_per_scope() {
    let ctx = Context::new();

    let provider_a = plugin("provider-a", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("db", "alpha".to_string()).await?;
            Ok(())
        })
    });
    let scoped_a = ctx.isolate("db");
    scoped_a
        .plugin(provider_a, None)
        .expect("spawn")
        .join()
        .await
        .expect("active");

    let provider_b = plugin("provider-b", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("db", "beta".to_string()).await?;
            Ok(())
        })
    });
    let scoped_b = ctx.isolate("db");
    scoped_b
        .plugin(provider_b, None)
        .expect("spawn")
        .join()
        .await
        .expect("active");

    // Consumers bound to each scope see their own realm.
    let consumer_a = scoped_a.inject(vec![Injection::new("db")], |ctx| {
        Box::pin(async move {
            assert_eq!(&*ctx.require::<String>("db")?, "alpha");
            Ok(())
        })
    });
    consumer_a.expect("spawn").join().await.expect("consumer a");

    let consumer_b = scoped_b.inject(vec![Injection::new("db")], |ctx| {
        Box::pin(async move {
            assert_eq!(&*ctx.require::<String>("db")?, "beta");
            Ok(())
        })
    });
    consumer_b.expect("spawn").join().await.expect("consumer b");

    // The root scope sees neither (strict lookup misses at default label).
    assert!(ctx.get::<String>("db").is_none());
}

#[tokio::test]
async fn intercept_config_merges_root_most_first() {
    let ctx = Context::new().intercept("svc", json!({"level": "root"}));
    let mid = ctx.intercept("svc", json!({"level": "mid", "extra": true}));
    assert_eq!(
        mid.service_config("svc"),
        json!({"level": "mid", "extra": true}),
        "innermost layer wins per key"
    );
    assert_eq!(
        ctx.service_config("svc"),
        json!({"level": "root"}),
        "parent scope unaffected by child intercept"
    );
}

#[tokio::test]
async fn config_update_restarts_with_new_config() {
    let ctx = Context::new();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let seen_in_plugin = Arc::clone(&seen);
    let plugin = plugin("configured", vec![], move |_ctx, config| {
        let seen = Arc::clone(&seen_in_plugin);
        Box::pin(async move {
            seen.lock().unwrap().push(config);
            Ok(())
        })
    });
    let handle = ctx.plugin(plugin, Some(json!({"v": 1}))).expect("spawn");
    handle.join().await.expect("active");
    handle.update(json!({"v": 2})).await.expect("update");
    handle.join().await.expect("re-active");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![json!({"v": 1}), json!({"v": 2})]
    );
}

#[tokio::test]
async fn parent_dispose_cascades_to_children() {
    let ctx = Context::new();

    let child = plugin("child", vec![], |_ctx, _c| Box::pin(async { Ok(()) }));
    let parent = plugin("parent", vec![], move |ctx, _c| {
        let child = std::sync::Arc::clone(&child);
        Box::pin(async move {
            ctx.plugin(child, None)?.join().await?;
            Ok(())
        })
    });

    let parent_handle = ctx.plugin(parent, None).expect("spawn parent");
    parent_handle.join().await.expect("parent active");
    assert_eq!(ctx.live_fibers(), 3, "root + parent + child");

    parent_handle.dispose().await;
    assert_eq!(ctx.live_fibers(), 1, "child cascaded away with the parent");
}
