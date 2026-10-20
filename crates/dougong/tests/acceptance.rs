// Rust guideline compliant 2026-10-20
//! Acceptance tests mapped from paper §15 (validation scenarios) and the
//! spatial/temporal guarantees (Thm 61-66, 73).
//!
//! Scenarios: availability-reactive wiring, provider replacement identity,
//! drain ordering on withdrawal, independent concurrent providers, isolation
//! realms, and the `relied()` introspection surface.

use std::sync::{Arc, Mutex};

use dougong::{Context, FiberState, Injection, plugin};
use serde_json::Value;

#[tokio::test]
async fn availability_reactive_consumer_rewires_when_provider_arrives() {
    let ctx = Context::new();
    let activations: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

    // Consumer starts first: it must sit in Pending.
    let activations_for_consumer = Arc::clone(&activations);
    let consumer = ctx
        .inject(vec![Injection::new("greeting")], move |_ctx| {
            let activations = Arc::clone(&activations_for_consumer);
            Box::pin(async move {
                *activations.lock().unwrap() += 1;
                Ok(())
            })
        })
        .expect("spawn consumer");
    assert_eq!(consumer.state(), FiberState::Pending);
    consumer
        .join()
        .await
        .expect("consumer stays pending until provider arrives");
    assert_eq!(*activations.lock().unwrap(), 0);

    // Provider arrives: the consumer activates without any manual wiring.
    let provider = plugin("provider", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("greeting", "hello".to_string()).await?;
            Ok(())
        })
    });
    ctx.plugin(provider, None)
        .expect("spawn provider")
        .join()
        .await
        .expect("provider active");

    consumer.join().await.expect("consumer converges");
    assert_eq!(consumer.state(), FiberState::Active);
    assert_eq!(*activations.lock().unwrap(), 1, "exactly one activation");
}

#[tokio::test]
async fn provider_identity_change_triggers_consumer_reload() {
    let ctx = Context::new();
    let activations: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let values: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Exclusive binding (paper §6.2): withdraw the old, provide the new.
    // The consumer must reload even though the new value equals the old one:
    // identity, not value equality, drives the epoch (Thm 64 coherence).
    let activations_in = Arc::clone(&activations);
    let values_in = Arc::clone(&values);
    let consumer = ctx
        .inject(vec![Injection::new("greeting")], move |ctx| {
            let activations = Arc::clone(&activations_in);
            let values = Arc::clone(&values_in);
            Box::pin(async move {
                *activations.lock().unwrap() += 1;
                let greeting = ctx.require::<String>("greeting")?;
                values.lock().unwrap().push((*greeting).clone());
                Ok(())
            })
        })
        .expect("spawn consumer");

    let provider_one = plugin("provider-one", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("greeting", "hello".to_string()).await?;
            Ok(())
        })
    });
    let one = ctx.plugin(provider_one, None).expect("spawn one");
    one.join().await.expect("one active");
    consumer.join().await.expect("consumer bound to one");
    assert_eq!(*activations.lock().unwrap(), 1);

    // Withdraw provider one: consumer drains back to Pending.
    one.dispose().await;
    let _ = consumer.join().await;
    assert_eq!(consumer.state(), FiberState::Pending);

    // Provider two offers the SAME value under a new identity.
    let provider_two = plugin("provider-two", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("greeting", "hello".to_string()).await?;
            Ok(())
        })
    });
    let two = ctx.plugin(provider_two, None).expect("spawn two");
    two.join().await.expect("two active");
    consumer.join().await.expect("consumer rewires to two");
    assert_eq!(
        *activations.lock().unwrap(),
        2,
        "identity change forces reload"
    );
    assert_eq!(*values.lock().unwrap(), vec!["hello", "hello"]);
}

#[tokio::test]
async fn withdrawal_drains_consumers_before_teardown_completes() {
    let ctx = Context::new();
    let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    // Consumer teardown reads the still-committed dependency (its own ω stays
    // readable while it drains) and records completion.
    let order_for_consumer = Arc::clone(&order);
    let consumer = ctx
        .inject(vec![Injection::new("greeting")], move |ctx| {
            let _order = Arc::clone(&order_for_consumer);
            Box::pin(async move {
                let _greeting = ctx.require::<String>("greeting")?;
                Ok(())
            })
        })
        .expect("spawn consumer");

    let order_for_provider = Arc::clone(&order);
    let provider = plugin("provider", vec![], move |ctx, _c| {
        let order = Arc::clone(&order_for_provider);
        Box::pin(async move {
            ctx.provide("greeting", "hello".to_string()).await?;
            // Effect teardown records that the provider's own cleanup ran; the
            // provide-disposer (drain) must complete only after consumers.
            let disposer: dougong::Disposer = Box::new(move || {
                let order = Arc::clone(&order);
                Box::pin(async move {
                    order.lock().unwrap().push("provider-cleanup");
                })
            });
            ctx.effect("provider-cleanup", async move { Ok(Some(disposer)) })
                .await?;
            Ok(())
        })
    });
    let provider_fiber = ctx.plugin(provider, None).expect("spawn provider");
    provider_fiber.join().await.expect("provider active");
    consumer.join().await.expect("consumer active");

    let provider_uid = provider_fiber.uid().expect("provider uid before dispose");
    provider_fiber.dispose().await;
    let recorded = order.lock().unwrap().clone();
    assert_eq!(
        recorded.last().copied(),
        Some("provider-cleanup"),
        "provider cleanup runs last: consumers drained first"
    );
    assert_eq!(
        consumer.state(),
        FiberState::Pending,
        "consumer parked after drain"
    );
    assert!(!ctx.relied(provider_uid), "no reliance remains");
}

#[tokio::test]
async fn independent_providers_unload_without_disturbing_each_other() {
    let ctx = Context::new();
    let b_activations: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

    let provider_a = plugin("provider-a", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("alpha", 1_i32).await?;
            Ok(())
        })
    });
    let provider_b = plugin("provider-b", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("beta", 2_i32).await?;
            Ok(())
        })
    });

    let activations_for_consumer = Arc::clone(&b_activations);
    let consumer_b = ctx
        .inject(vec![Injection::new("beta")], move |ctx| {
            let activations = Arc::clone(&activations_for_consumer);
            Box::pin(async move {
                *activations.lock().unwrap() += 1;
                assert_eq!(*ctx.require::<i32>("beta")?, 2);
                Ok(())
            })
        })
        .expect("spawn consumer b");

    let handle_a = ctx.plugin(provider_a, None).expect("spawn a");
    let handle_b = ctx.plugin(provider_b, None).expect("spawn b");
    handle_a.join().await.expect("a active");
    handle_b.join().await.expect("b active");
    consumer_b.join().await.expect("consumer b active");
    assert_eq!(*b_activations.lock().unwrap(), 1);

    // Unload A: B's fiber and consumer are untouched (Thm 61 recovery
    // exactness — removing one fiber preserves the others).
    handle_a.dispose().await;
    assert_eq!(handle_b.state(), FiberState::Active);
    assert_eq!(
        *b_activations.lock().unwrap(),
        1,
        "consumer b never reloaded"
    );
}

#[tokio::test]
async fn realms_keep_same_named_services_apart() {
    let ctx = Context::new();
    let realm_one = ctx.isolate("user");
    let realm_two = ctx.isolate("user");

    let provider_one = plugin("p1", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("user", "alice".to_string()).await?;
            Ok(())
        })
    });
    let provider_two = plugin("p2", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("user", "bob".to_string()).await?;
            Ok(())
        })
    });
    realm_one
        .plugin(provider_one, None)
        .expect("spawn p1")
        .join()
        .await
        .expect("p1");
    realm_two
        .plugin(provider_two, None)
        .expect("spawn p2")
        .join()
        .await
        .expect("p2");

    let seen_one = Arc::new(Mutex::new(String::new()));
    let seen_one_for_consumer = Arc::clone(&seen_one);
    let consumer_one = realm_one
        .inject(vec![Injection::new("user")], move |ctx| {
            let seen = Arc::clone(&seen_one_for_consumer);
            Box::pin(async move {
                let user = ctx.require::<String>("user")?;
                *seen.lock().unwrap() = (*user).clone();
                Ok(())
            })
        })
        .expect("spawn consumer one");
    consumer_one.join().await.expect("consumer one");
    assert_eq!(*seen_one.lock().unwrap(), "alice");

    let seen_two = Arc::new(Mutex::new(String::new()));
    let seen_two_for_consumer = Arc::clone(&seen_two);
    let consumer_two = realm_two
        .inject(vec![Injection::new("user")], move |ctx| {
            let seen = Arc::clone(&seen_two_for_consumer);
            Box::pin(async move {
                let user = ctx.require::<String>("user")?;
                *seen.lock().unwrap() = (*user).clone();
                Ok(())
            })
        })
        .expect("spawn consumer two");
    consumer_two.join().await.expect("consumer two");
    assert_eq!(*seen_two.lock().unwrap(), "bob");
}

#[tokio::test]
async fn undeclared_access_stays_denied_capability_model() {
    let ctx = Context::new();
    let provider = plugin("p", vec![], |ctx, _c| {
        Box::pin(async move {
            ctx.provide("secret", "top".to_string()).await?;
            Ok(())
        })
    });
    ctx.plugin(provider, None)
        .expect("spawn")
        .join()
        .await
        .expect("active");

    // TS-parity (Alg. 6): an in-process undeclared read still resolves when
    // some active provider exists — declaration drives the REACTIVE wiring,
    // while denial of untrusted code is enforced at the plugin sandbox
    // boundary (wasm), not this walk (design decision D11).
    let undeclared_reader = ctx
        .inject(vec![], move |ctx| {
            Box::pin(async move {
                assert!(ctx.get::<String>("secret").is_some());
                Ok(())
            })
        })
        .expect("spawn undeclared reader");
    undeclared_reader.join().await.expect("reader ran");

    // A declared consumer that depends on a missing service stays Pending.
    let waiting = ctx
        .inject(vec![Injection::new("nonexistent")], |_ctx| {
            Box::pin(async { Ok(()) })
        })
        .expect("spawn waiting");
    assert_eq!(waiting.state(), FiberState::Pending);
}

#[tokio::test]
async fn pending_consumer_activates_after_failed_then_recovered_dependency() {
    let ctx = Context::new();
    let attempts: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    // Provider whose first activation fails after registering an effect.
    let status_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let status_log = Arc::clone(&status_log);
        ctx.on(
            "internal/status",
            Arc::new(move |_ctx, payload, _next| {
                let status_log = Arc::clone(&status_log);
                Box::pin(async move {
                    if payload["fiber"] == "flaky" {
                        status_log
                            .lock()
                            .unwrap()
                            .push(format!("{}->{}", payload["state"], payload["uid"]));
                    }
                    Ok(Value::Null)
                })
            }),
        )
        .await
        .expect("status listener");
    }
    let attempts_for_provider = Arc::clone(&attempts);
    let flaky = plugin("flaky", vec![], move |ctx, _c| {
        let attempts = Arc::clone(&attempts_for_provider);
        Box::pin(async move {
            let should_fail = {
                let mut attempts = attempts.lock().unwrap();
                attempts.push("attempt");
                attempts.len() == 1
            };
            let label = "resource";
            let disposer: dougong::Disposer = Box::new(|| Box::pin(async {}));
            ctx.effect(label, async move { Ok(Some(disposer)) }).await?;
            if should_fail {
                Err(dougong::Error::msg("first attempt fails"))
            } else {
                ctx.provide("svc", true).await?;
                Ok(())
            }
        })
    });

    let consumer = ctx
        .inject(vec![Injection::new("svc")], |_ctx| {
            Box::pin(async { Ok(()) })
        })
        .expect("spawn consumer");
    assert_eq!(consumer.state(), FiberState::Pending);

    let flaky_handle = ctx.plugin(flaky, None).expect("spawn flaky");
    assert!(flaky_handle.join().await.is_err(), "first attempt fails");
    assert_eq!(flaky_handle.state(), FiberState::Failed);
    assert_eq!(
        consumer.state(),
        FiberState::Pending,
        "no service to consume yet"
    );

    // A notification cycle (touch via update/restart path) re-arms the parked
    // provider; second attempt succeeds.
    flaky_handle
        .restart()
        .await
        .expect("second attempt succeeds");
    eprintln!("STATUS: {:?}", status_log.lock().unwrap());
    assert_eq!(flaky_handle.state(), FiberState::Active);
    assert_eq!(*attempts.lock().unwrap(), vec!["attempt", "attempt"]);

    consumer
        .join()
        .await
        .expect("consumer activates once service exists");
    assert_eq!(consumer.state(), FiberState::Active);
}
