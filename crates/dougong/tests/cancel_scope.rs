// Rust guideline compliant 2026-10-24
//! Cancel-scope behavior (stability track S2): unloading or disposing a
//! fiber reaps every task it owns — in-flight listener invocations never
//! outlive teardown, and a hung plugin body can no longer park `dispose`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, FiberHandle, FiberState, Listener, plugin};
use serde_json::{Value, json};

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: Duration = Duration::from_secs(5);

/// Records when a dropped future's captured state is released.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Polls the condition with a hard ceiling: a hang fails, never flakes.
async fn wait_until(condition: impl Fn() -> bool + Send + Sync) {
    tokio::time::timeout(RAIL, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition reached");
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

#[tokio::test]
async fn dispose_unblocks_a_hung_plugin_body() {
    let ctx = Context::new();
    let started = Arc::new(AtomicBool::new(false));
    let handle = ctx
        .plugin(hung_body_plugin(Arc::clone(&started)), None)
        .expect("spawn hung plugin");
    wait_until(|| started.load(Ordering::SeqCst)).await;
    // The fiber is Loading with its body parked forever; dispose must still
    // settle because the scope aborts the body at its yield point.
    tokio::time::timeout(RAIL, handle.dispose())
        .await
        .expect("dispose settles despite the hung body");
    assert_eq!(handle.state(), FiberState::Disposed);
}

#[tokio::test]
async fn dispose_cancels_an_in_flight_listener() {
    let ctx = Context::new();
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let plugin = listener_plugin("listener", "test/evt", {
        let started = Arc::clone(&started);
        let dropped = Arc::clone(&dropped);
        move || {
            let flag = DropFlag(Arc::clone(&dropped));
            started.store(true, Ordering::SeqCst);
            Box::pin(async move {
                let _flag = flag;
                let () = std::future::pending().await;
                Ok(Value::Null)
            })
        }
    });
    let handle = ctx.plugin(plugin, None).expect("spawn listener plugin");
    handle.join().await.expect("listener plugin active");
    ctx.emit("test/evt", Value::Null);
    wait_until(|| started.load(Ordering::SeqCst)).await;
    tokio::time::timeout(RAIL, handle.dispose())
        .await
        .expect("dispose settles despite the in-flight listener");
    assert!(
        dropped.load(Ordering::SeqCst),
        "the scope dropped the in-flight listener future"
    );
    assert_eq!(handle.state(), FiberState::Disposed);
}

#[tokio::test]
async fn restart_cancels_the_old_generation_and_rearms_a_fresh_one() {
    let ctx = Context::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_dropped = Arc::new(AtomicBool::new(false));
    let plugin = listener_plugin("listener", "test/restart", {
        let calls = Arc::clone(&calls);
        let first_dropped = Arc::clone(&first_dropped);
        move || {
            let calls = Arc::clone(&calls);
            let first_dropped = Arc::clone(&first_dropped);
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    // The first activation's invocation parks until the
                    // unload reap aborts it.
                    let _flag = DropFlag(first_dropped);
                    let () = std::future::pending().await;
                }
                Ok(Value::Null)
            })
        }
    });
    let handle = ctx.plugin(plugin, None).expect("spawn listener plugin");
    handle.join().await.expect("active");
    ctx.emit("test/restart", Value::Null);
    wait_until(|| calls.load(Ordering::SeqCst) == 1).await;
    tokio::time::timeout(RAIL, handle.restart())
        .await
        .expect("restart settles despite the in-flight invocation")
        .expect("restart ok");
    assert!(
        first_dropped.load(Ordering::SeqCst),
        "the old generation's in-flight invocation was aborted"
    );
    // The fresh generation accepts dispatches again.
    ctx.emit("test/restart", Value::Null);
    wait_until(|| calls.load(Ordering::SeqCst) >= 2).await;
}

#[tokio::test]
async fn parallel_dispatch_skips_a_listener_unloaded_mid_flight() {
    let ctx = Context::new();
    let slow_started = Arc::new(AtomicBool::new(false));
    let slow = listener_plugin("slow", "test/par", {
        let slow_started = Arc::clone(&slow_started);
        move || {
            slow_started.store(true, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    });
    let fast = listener_plugin("fast", "test/par", || Box::pin(async { Ok(json!("fast")) }));
    let slow_handle = ctx.plugin(slow, None).expect("spawn slow");
    slow_handle.join().await.expect("slow active");
    let fast_handle = ctx.plugin(fast, None).expect("spawn fast");
    fast_handle.join().await.expect("fast active");

    let dispatcher = {
        let ctx = ctx.clone();
        tokio::spawn(async move { ctx.parallel("test/par", Value::Null).await })
    };
    wait_until(|| slow_started.load(Ordering::SeqCst)).await;
    slow_handle.dispose().await;
    // The unloaded listener's in-flight task resolves to "absent", not to a
    // failure the emitter would have to handle.
    let outcome = tokio::time::timeout(RAIL, dispatcher)
        .await
        .expect("parallel dispatch settles")
        .expect("dispatcher task joined")
        .expect("no failure aggregated for the vanished listener");
    assert_eq!(outcome, json!("fast"));
}

#[tokio::test]
async fn serial_skips_a_listener_disposed_mid_chain() {
    let ctx = Context::new();
    let second_ran = Arc::new(AtomicBool::new(false));
    let slot: Arc<Mutex<Option<FiberHandle>>> = Arc::new(Mutex::new(None));
    let disposer = listener_plugin("disposer", "test/seq", {
        let slot = Arc::clone(&slot);
        move || {
            let slot = Arc::clone(&slot);
            Box::pin(async move {
                // Take the handle before the await: a std MutexGuard is not
                // Send and must not live in the future's state.
                let handle = slot.lock().expect("handle slot").take();
                if let Some(handle) = handle {
                    handle.dispose().await;
                }
                Ok(Value::Null)
            })
        }
    });
    let second = listener_plugin("second", "test/seq", {
        let second_ran = Arc::clone(&second_ran);
        move || {
            second_ran.store(true, Ordering::SeqCst);
            Box::pin(async move { Ok(Value::Null) })
        }
    });
    let first = ctx.plugin(disposer, None).expect("spawn disposer");
    first.join().await.expect("disposer active");
    let second_handle = ctx.plugin(second, None).expect("spawn second");
    second_handle.join().await.expect("second active");
    slot.lock().expect("handle slot").replace(second_handle);
    // The first listener disposes the second's fiber mid-dispatch; the
    // second entry was snapshotted but must be skipped, not run dead code.
    tokio::time::timeout(RAIL, ctx.serial("test/seq", Value::Null))
        .await
        .expect("serial settles")
        .expect("serial ok");
    assert!(
        !second_ran.load(Ordering::SeqCst),
        "the disposed listener was skipped"
    );
}

#[tokio::test]
async fn waterfall_dead_listener_cannot_veto_the_chain() {
    let ctx = Context::new();
    let second_ran = Arc::new(AtomicBool::new(false));
    let slot: Arc<Mutex<Option<FiberHandle>>> = Arc::new(Mutex::new(None));
    // Waterfall listeners wrap `next`; the advancing listener disposes the
    // second listener's fiber mid-chain and then continues the chain.
    let advancing = plugin("advancing", vec![], {
        let slot = Arc::clone(&slot);
        move |ctx, _config| {
            let slot = Arc::clone(&slot);
            Box::pin(async move {
                let listener: Listener = Arc::new(move |_ctx, payload, next| {
                    let slot = Arc::clone(&slot);
                    Box::pin(async move {
                        // Take the handle before the await: a std MutexGuard
                        // is not Send and must not live in the future.
                        let handle = slot.lock().expect("handle slot").take();
                        if let Some(handle) = handle {
                            handle.dispose().await;
                        }
                        next.call(payload).await
                    })
                });
                ctx.on("test/wf2", listener).await?;
                Ok(())
            })
        }
    });
    let second = listener_plugin("second", "test/wf2", {
        let second_ran = Arc::clone(&second_ran);
        move || {
            second_ran.store(true, Ordering::SeqCst);
            Box::pin(async move { Ok(Value::Null) })
        }
    });
    let advance = ctx.plugin(advancing, None).expect("spawn advancing");
    advance.join().await.expect("advancing active");
    let second_handle = ctx.plugin(second, None).expect("spawn second");
    second_handle.join().await.expect("second active");
    slot.lock().expect("handle slot").replace(second_handle);
    let outcome = tokio::time::timeout(RAIL, async {
        ctx.waterfall("test/wf2", Value::Null, |payload| {
            Box::pin(async move { Ok(json!({ "fallback": payload })) })
        })
        .await
    })
    .await
    .expect("waterfall settles")
    .expect("waterfall ok");
    // The dead listener neither ran nor vetoed: the fallback produced it.
    assert!(!second_ran.load(Ordering::SeqCst));
    assert_eq!(outcome, json!({ "fallback": Value::Null }));
}

#[tokio::test]
async fn stop_settles_with_tasks_in_flight() {
    let ctx = Context::new();
    let body_started = Arc::new(AtomicBool::new(false));
    let listener_started = Arc::new(AtomicBool::new(false));
    let hung = ctx
        .plugin(hung_body_plugin(Arc::clone(&body_started)), None)
        .expect("spawn hung plugin");
    let listener = ctx
        .plugin(
            listener_plugin("listener", "test/stop", {
                let listener_started = Arc::clone(&listener_started);
                move || {
                    listener_started.store(true, Ordering::SeqCst);
                    Box::pin(std::future::pending())
                }
            }),
            None,
        )
        .expect("spawn listener plugin");
    listener.join().await.expect("listener active");
    ctx.emit("test/stop", Value::Null);
    wait_until(|| body_started.load(Ordering::SeqCst) && listener_started.load(Ordering::SeqCst))
        .await;
    tokio::time::timeout(RAIL, ctx.stop())
        .await
        .expect("stop settles with a hung body and a parked listener in flight");
    assert_eq!(hung.state(), FiberState::Disposed);
    assert_eq!(listener.state(), FiberState::Disposed);
}
