// Rust guideline compliant 2026-10-21
//! End-to-end wasm plugin demo: loads the `greeter_wasm` component, binds an
//! in-process consumer through the dynamic channel, exercises events and
//! waterfall across the boundary, and verifies drain plus teardown.

use std::path::PathBuf;
use std::sync::Arc;

use dougong::{Context, Injection, Invoker, LogLevel, plugin};
use dougong_wasm::{WasmHost, WasmLimits, load};

fn component_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOUGONG_GREETER_WASM") {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend([
        "../../",
        "target",
        "wasm32-wasip2",
        "debug",
        "greeter_wasm.wasm",
    ]);
    path
}

fn identity_fallback(
    payload: serde_json::Value,
) -> dougong::BoxFuture<dougong::Result<serde_json::Value>> {
    Box::pin(async move { Ok(payload) })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = component_path();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let host = WasmHost::new(WasmLimits::default())?;
        let greeter: Arc<dyn dougong::Plugin> = load(&host, &path).await?;

        let ctx = Context::new();
        let logger = ctx.logger();
        let log = |message: String| {
            logger.log(LogLevel::Info, "wasm-demo", &message);
        };

        log("-- loaded wasm component --".to_string());

        // Consumer declares `greeting` before the provider exists: it parks
        // in Pending and rewires when the component provides.
        let consumer = plugin("consumer", vec![Injection::new("greeting")], {
            move |ctx, _config| {
                Box::pin(async move {
                    let invoker = ctx.require::<Invoker>("greeting")?;
                    let payload = invoker.call(0, Vec::new()).await?;
                    let text = String::from_utf8(payload)
                        .map_err(|err| dougong::Error::msg(err.to_string()))?;
                    ctx.logger().log(
                        LogLevel::Info,
                        "wasm-demo",
                        &format!("consumer received: {text}"),
                    );
                    Ok(())
                })
            }
        });

        let consumer_fiber = ctx.plugin(consumer, None)?;
        let greeter_fiber = ctx.plugin(greeter, None)?;

        greeter_fiber.join().await?;
        consumer_fiber.join().await?;

        // Events across the boundary: a serial listener and a waterfall
        // listener, both implemented inside the component.
        let pong = ctx.serial("ping", serde_json::json!({ "seq": 1 })).await?;
        log(format!("serial ping reply: {pong}"));
        assert_eq!(pong, serde_json::json!("pong from greeter-wasm"));

        let loud = ctx
            .waterfall(
                "upper",
                serde_json::json!({ "text": "make me loud" }),
                identity_fallback,
            )
            .await?;
        log(format!("waterfall upper reply: {loud}"));
        assert!(loud.to_string().contains("MAKE ME LOUD"));

        // Withdrawal drains the consumer before the guest teardown runs.
        greeter_fiber.dispose().await;
        assert_eq!(consumer_fiber.state(), dougong::FiberState::Pending);
        log("-- consumer drained and parked --".to_string());

        ctx.stop().await;
        log("-- wasm demo done --".to_string());
        Ok::<(), dougong::Error>(())
    })?;
    Ok(())
}
