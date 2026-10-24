// Rust guideline compliant 2026-10-24
//! End-to-end dylib plugin demo: loads `libgreeter_plugin`, binds an
//! in-process consumer through the dynamic channel, and verifies the call
//! plus the teardown ordering. On the ABI v3 surface it also proves the
//! plugin's outbound legs: greeter's own worker thread requires the
//! in-process `greeter.oracle` service during activation and publishes
//! `greeter/ready` on the kernel bus when done.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, Invoke, Invoker, LogLevel, plugin};
use dougong_dylib::load;
use greeter_plugin::{GREETER_HELLO, GREETER_WORKER_REPLY};

fn plugin_path() -> PathBuf {
    // The cdylib lands next to the other workspace artifacts.
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend(["../../", "target", "debug"]);
    let name = if cfg!(target_os = "macos") {
        "libgreeter_plugin.dylib"
    } else if cfg!(windows) {
        "greeter_plugin.dll"
    } else {
        "libgreeter_plugin.so"
    };
    path.join(name)
}

/// Satisfies greeter's declared `greeter.oracle` inject: echoes back
/// `oracle:<payload>`. The reentrancy arm lives in `outbound_demo`; here the
/// oracle only needs the success path.
struct Oracle;

impl Invoke for Oracle {
    fn call(&self, _method: u32, payload: Vec<u8>) -> dougong::BoxFuture<dougong::Result<Vec<u8>>> {
        Box::pin(async move { Ok([b"oracle:".as_ref(), &payload].concat()) })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = plugin_path();
    let greeter: Arc<dyn dougong::Plugin> = load(&path)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let ctx = Context::new();
        let logger = ctx.logger();
        let log = |message: String| {
            logger.log(LogLevel::Info, "dylib-demo", &message);
        };

        log("-- loaded dylib plugin --".to_string());

        // The oracle must be committed before greeter activates: the plugin's
        // worker thread requires it from `setup` time onward.
        let oracle = plugin("oracle", vec![], |ctx, _config| {
            Box::pin(async move {
                ctx.provide("greeter.oracle", Invoker::new(Arc::new(Oracle)))
                    .await?;
                Ok(())
            })
        });
        let oracle_fiber = ctx.plugin(oracle, None)?;
        oracle_fiber.join().await?;

        // Registered before greeter loads: the worker's `greeter/ready` emit
        // is fire-and-forget, so the listener must already be in place. The
        // oneshot buffers the payload until the demo awaits it below.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
        ctx.on(
            "greeter/ready",
            Arc::new(move |_ctx, payload, _next| {
                let ready_tx = Arc::clone(&ready_tx);
                Box::pin(async move {
                    if let Ok(mut slot) = ready_tx.lock()
                        && let Some(tx) = slot.take()
                    {
                        let _dropped = tx.send(payload);
                    }
                    Ok(serde_json::Value::Null)
                })
            }),
        )
        .await?;

        // Consumer declares `greeting` before the provider exists: it parks
        // in Pending and rewires when the dylib fiber provides.
        let consumer = plugin("consumer", vec![dougong::Injection::new("greeting")], {
            move |ctx, _config| {
                Box::pin(async move {
                    let invoker = ctx.require::<Invoker>("greeting")?;
                    let payload = invoker.call(GREETER_HELLO, Vec::new()).await?;
                    let text = String::from_utf8(payload)
                        .map_err(|err| dougong::Error::msg(err.to_string()))?;
                    ctx.logger().log(
                        LogLevel::Info,
                        "dylib-demo",
                        &format!("consumer received: {text}"),
                    );
                    assert_eq!(text, "hello from a dougong dylib");
                    Ok(())
                })
            }
        });

        let consumer_fiber = ctx.plugin(consumer, None)?;
        let greeter_fiber = ctx.plugin(greeter, None)?;

        greeter_fiber.join().await?;
        consumer_fiber.join().await?;

        // ABI v3 outbound leg 1: the worker thread's `greeter/ready` emit
        // arrived on the bus (it fires after the worker's require completed).
        let ready = tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .map_err(|err| dougong::Error::msg(format!("greeter/ready did not arrive: {err}")))?
            .map_err(|err| dougong::Error::msg(format!("greeter/ready listener dropped: {err}")))?;
        assert_eq!(
            ready.get("source").and_then(serde_json::Value::as_str),
            Some("greeter-worker")
        );
        log("-- own-thread emit reached the bus --".to_string());

        // Outbound leg 2: the worker's synchronous `require` reply is read
        // back through the plugin's own service.
        let greeting = ctx
            .get::<Invoker>("greeting")
            .ok_or_else(|| dougong::Error::msg("greeting is not committed"))?;
        let reply = greeting.call(GREETER_WORKER_REPLY, Vec::new()).await?;
        assert_eq!(reply, b"oracle:worker-hello");
        log("-- own-thread require reply read back --".to_string());

        // ABI v2 event surface: the dylib greeter listens to `ping` (echo)
        // and `upper` (waterfall pull through STATUS_DEFERRED -> on_next).
        let pong = ctx.serial("ping", serde_json::json!("ping")).await?;
        assert_eq!(pong, serde_json::json!("pong from greeter-dylib"));
        log("-- event ping echoed through the dylib bridge --".to_string());

        let loud = ctx
            .waterfall("upper", serde_json::json!("make me loud"), |payload| {
                Box::pin(async move { Ok(payload) })
            })
            .await?;
        assert_eq!(loud, serde_json::json!("MAKE ME LOUD"));
        log("-- waterfall pulled through on_next --".to_string());

        // Withdrawal drains the consumer before the provider teardown runs.
        greeter_fiber.dispose().await;
        assert_eq!(consumer_fiber.state(), dougong::FiberState::Pending);
        log("-- consumer drained and parked --".to_string());

        ctx.stop().await;
        log("-- dylib demo done --".to_string());
        Ok::<(), dougong::Error>(())
    })?;
    Ok(())
}
