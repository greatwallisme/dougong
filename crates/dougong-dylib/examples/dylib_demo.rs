// Rust guideline compliant 2026-10-20
//! End-to-end dylib plugin demo: loads `libgreeter_plugin`, binds an
//! in-process consumer through the dynamic channel, and verifies the call
//! plus the teardown ordering.

use std::path::PathBuf;
use std::sync::Arc;

use dougong::{Context, Invoker, LogLevel, plugin};
use dougong_dylib::load;
use greeter_plugin::GREETER_HELLO;

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
