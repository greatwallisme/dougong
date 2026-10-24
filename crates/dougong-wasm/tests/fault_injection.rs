// Rust guideline compliant 2026-10-24
//! Boundary fault injection with the real `greeter_wasm` component (S7):
//! a guest trap poisons its component instance — every later entry fails
//! with a deterministic transport error (wasmtime forbids re-entry after
//! a trap) while consumers, the provider fiber, and the kernel survive;
//! guest listener failures settle per class through the five-mode
//! dispatch contract; a guest teardown trap never blocks the unload (the
//! adapter treats foreign teardown as best-effort — `Disposer` carries
//! no error channel).
//!
//! The failure arms live in the guest itself (payload `"boom"` traps,
//! `"fail"` returns a guest error, config `{"trap_teardown":true}` arms
//! the teardown trap); the demo paths never send those payloads.
//!
//! These tests need the `wasm32-wasip2` artifact. When it is absent (a
//! fresh checkout) they skip, so `cargo test --workspace` stays green
//! before `cargo build -p greeter-wasm --target wasm32-wasip2` has run;
//! the release-time end-to-end verification always builds the guest first.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, Injection, Invoker, Plugin};
use dougong_wasm::{WasmHost, WasmLimits, load};
use serde_json::{Value, json};

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: Duration = Duration::from_secs(5);

/// Resolves the guest component (`DOUGONG_GREETER_WASM` first, then the
/// workspace target dir), or `None` when it has not been built.
fn component_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("DOUGONG_GREETER_WASM") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend([
        "../../",
        "target",
        "wasm32-wasip2",
        "debug",
        "greeter_wasm.wasm",
    ]);
    path.exists().then_some(path)
}

/// Boots a root context with the greeter component loaded and active, or
/// returns `None` when the artifact is unavailable.
async fn boot(config: Option<Value>) -> Option<(Context, dougong::FiberHandle, Arc<dyn Plugin>)> {
    let path = component_path()?;
    let host = WasmHost::new(WasmLimits::default()).expect("wasm host");
    let plugin = load(&host, &path).await.expect("component loads");
    let ctx = Context::new();
    let handle = ctx
        .plugin(Arc::clone(&plugin), config)
        .expect("spawn wasm plugin");
    tokio::time::timeout(RAIL, handle.join())
        .await
        .expect("activation settles")
        .expect("activation ok");
    Some((ctx, handle, plugin))
}

/// Registers a counting listener for `event` on the root fiber.
async fn count_listener(ctx: &Context, event: &str) -> Arc<AtomicUsize> {
    let hits = Arc::new(AtomicUsize::new(0));
    let inner = Arc::clone(&hits);
    ctx.on(
        event,
        Arc::new(move |_ctx, _payload, _next| {
            inner.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(Value::Null) })
        }),
    )
    .await
    .expect("register listener");
    hits
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_service_call_trap_surfaces_transport_and_consumers_survive() {
    let Some((ctx, provider, _plugin)) = boot(None).await else {
        return;
    };

    // A consumer calls the component through the dynamic channel twice:
    // the trap payload, then a normal one. The sandbox contains the trap
    // to the poisoned instance — every later entry into it fails with a
    // deterministic transport error (wasmtime forbids re-entering a
    // component instance after a trap), while the fibers live on.
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let consumer_outcomes = Arc::clone(&outcomes);
    let consumer = ctx
        .plugin(
            dougong::plugin(
                "consumer",
                vec![Injection::new("greeting")],
                move |ctx, _config| {
                    let outcomes = Arc::clone(&consumer_outcomes);
                    Box::pin(async move {
                        let invoker = ctx.require::<Invoker>("greeting")?;
                        let trapped = invoker.call(0, b"boom".to_vec()).await;
                        let reentry = invoker.call(0, Vec::new()).await;
                        let trapped = trapped.expect_err("the trap payload fails the call");
                        let reentry = reentry.expect_err("a trapped instance stays dead");
                        outcomes.lock().expect("outcomes").push((
                            trapped.code.to_string(),
                            reentry.code.to_string(),
                            reentry.to_string(),
                        ));
                        Ok(())
                    })
                },
            ),
            None,
        )
        .expect("spawn consumer");
    tokio::time::timeout(RAIL, consumer.join())
        .await
        .expect("consumer settles")
        .expect("consumer body ok");

    let outcomes = outcomes.lock().expect("outcomes").clone();
    let [(first, second, message)] = outcomes.as_slice() else {
        panic!("expected exactly one consumer outcome") // contract: the body pushes once
    };
    assert_eq!(first, "wasm_trap");
    assert_eq!(second, "wasm_trap");
    assert!(
        message.contains("cannot enter component instance"),
        "re-entry after a trap is a deterministic transport error: {message}"
    );
    assert_eq!(provider.state(), dougong::FiberState::Active);
    assert_eq!(consumer.state(), dougong::FiberState::Active);

    // Disposing the poisoned instance completes even though its teardown
    // traps as well (best-effort foreign teardown), freeing the slot.
    tokio::time::timeout(RAIL, provider.dispose())
        .await
        .expect("dispose of the poisoned instance settles");
    assert_eq!(provider.state(), dougong::FiberState::Disposed);

    // The blast radius is the poisoned instance only: a fresh component
    // instance on a fresh host provides and answers normally.
    let path = component_path().expect("artifact present (boot succeeded)");
    let host = WasmHost::new(WasmLimits::default()).expect("wasm host");
    let plugin: Arc<dyn Plugin> = load(&host, &path).await.expect("component loads");
    let second = ctx.plugin(plugin, None).expect("spawn second instance");
    tokio::time::timeout(RAIL, second.join())
        .await
        .expect("second activation settles")
        .expect("second activation ok");
    let invoker = ctx.get::<Invoker>("greeting").expect("service committed");
    let reply = tokio::time::timeout(RAIL, invoker.call(0, Vec::new()))
        .await
        .expect("call settles")
        .expect("fresh instance answers");
    assert!(reply.starts_with(b"hello from a wasm component"));
    ctx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_listener_failure_arms_settle_per_class() {
    let Some((ctx, _provider, _plugin)) = boot(None).await else {
        return;
    };
    let hits = count_listener(&ctx, "ping").await;

    // Guest error arm: the listener returns an error (logic class); the
    // dispatch aggregates it and the healthy in-process listener on the
    // same event is still delivered.
    let err = ctx
        .parallel("ping", json!("fail"))
        .await
        .expect_err("the failing guest listener aggregates");
    assert_eq!(err.code, "aggregate");
    assert!(
        err.to_string().contains("listener failure"),
        "guest error reaches the aggregate: {err}"
    );
    assert_eq!(err.class, dougong::ErrorClass::Logic);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "good listener delivered");

    // Trap arm: the guest traps mid-listener; the failure is transport
    // class and still just one aggregate entry — the kernel is unaffected.
    let err = ctx
        .parallel("ping", json!("boom"))
        .await
        .expect_err("the trapping guest listener aggregates");
    assert_eq!(err.code, "aggregate");
    assert!(
        err.to_string().contains("wasm_trap"),
        "trap surfaces as transport: {err}"
    );
    assert_eq!(err.class, dougong::ErrorClass::Transport);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "good listener delivered again"
    );

    // The trapped instance is poisoned: every later entry fails with a
    // deterministic transport error (wasmtime forbids re-entry after a
    // trap), while the kernel's dispatch machinery and the healthy
    // in-process listener carry on unaffected.
    let err = ctx
        .parallel("ping", json!("hello"))
        .await
        .expect_err("the poisoned instance keeps failing deterministically");
    assert!(err.to_string().contains("cannot enter component instance"));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "good listener still delivered"
    );
    ctx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wasm_teardown_trap_does_not_block_unload() {
    let Some((ctx, handle, _plugin)) = boot(Some(json!({"trap_teardown": true}))).await else {
        return;
    };

    // The guest traps inside its teardown (`run_disposer(0)`). Teardown is
    // best-effort across the boundary — the sandbox must not let an
    // untrusted plugin park its own unload (D11).
    tokio::time::timeout(RAIL, handle.dispose())
        .await
        .expect("dispose is not parked by the trapping teardown");
    assert_eq!(handle.state(), dougong::FiberState::Disposed);

    // The kernel is unaffected: a fresh component instance still boots and
    // serves.
    let path = component_path().expect("artifact present (boot succeeded)");
    let host = WasmHost::new(WasmLimits::default()).expect("wasm host");
    let plugin: Arc<dyn Plugin> = load(&host, &path).await.expect("component loads");
    let second = ctx.plugin(plugin, None).expect("spawn second instance");
    tokio::time::timeout(RAIL, second.join())
        .await
        .expect("second activation settles")
        .expect("second activation ok");
    ctx.stop().await;
}
