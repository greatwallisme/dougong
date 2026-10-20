// Rust guideline compliant 2026-10-21
//! Declarative loader demo: one JSON document drives the full lifecycle —
//! a builtin plugin plus the dylib greeter artifact, a config hot-update,
//! and a retirement pass.

use std::collections::HashMap;
use std::path::PathBuf;

use dougong::{Context, LogLevel, plugin};
use dougong_loader::{Config, Reconciler};

fn dylib_path() -> PathBuf {
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let ctx = Context::new();
        let logger = ctx.logger();
        let log = |message: String| {
            logger.log(LogLevel::Info, "loader-demo", &message);
        };

        // A built-in plugin of the harness, managed by the same document
        // as external artifacts.
        let boot = plugin("boot", vec![], |ctx, config| {
            Box::pin(async move {
                ctx.logger().log(
                    LogLevel::Info,
                    "boot",
                    &format!("boot plugin active with config {config}"),
                );
                Ok(())
            })
        });
        let builtins = HashMap::from([("boot".to_string(), boot)]);

        let mut reconciler = Reconciler::new(builtins, None);

        let doc = format!(
            r#"{{"plugins":[
                {{"name":"boot","source":{{"kind":"builtin","name":"boot"}},"config":{{"phase":"boot"}}}},
                {{"name":"greeter","source":{{"kind":"dylib","path":"{}"}}}}
            ]}}"#,
            dylib_path().display()
        );
        reconciler.replace(Config::parse(&doc)?);
        let report = reconciler.apply(&ctx).await;
        log(format!("pass 1: {report:?}"));
        for live in reconciler.live().values() {
            live.handle.join().await?;
        }

        // Hot-update the builtin's config; the dylib slot is unchanged.
        let doc = format!(
            r#"{{"plugins":[
                {{"name":"boot","source":{{"kind":"builtin","name":"boot"}},"config":{{"phase":"steady"}}}},
                {{"name":"greeter","source":{{"kind":"dylib","path":"{}"}}}}
            ]}}"#,
            dylib_path().display()
        );
        reconciler.replace(Config::parse(&doc)?);
        let report = reconciler.apply(&ctx).await;
        log(format!("pass 2: {report:?}"));
        for live in reconciler.live().values() {
            live.handle.join().await?;
        }

        // Retire everything; a follow-up pass over the empty document is
        // then fully converged.
        reconciler.replace(Config::parse(r#"{"plugins":[]}"#)?);
        let report = reconciler.apply(&ctx).await;
        log(format!("pass 3: {report:?}"));
        assert_eq!(report.retired.len(), 2);
        let report = reconciler.apply(&ctx).await;
        assert!(report.converged());

        ctx.stop().await;
        log("-- loader demo done --".to_string());
        Ok::<(), dougong::Error>(())
    })?;
    Ok(())
}
