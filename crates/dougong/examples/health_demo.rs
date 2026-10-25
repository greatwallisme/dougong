// Rust guideline compliant 2026-10-25
//! Health endpoint body demo (O5, improve 三.5). Phase 0 ruling OBS puts the
//! HTTP wiring in the host: a real deployment serves the health snapshot at
//! `/healthz` / `/readyz`. This example shows the body composition a host
//! would perform — it builds the JSON from `Context::health()` and prints it
//! through the kernel logger, first for a healthy world, then after one
//! plugin failed activation.

use dougong::{Context, LogLevel, plugin};
use serde_json::json;

#[tokio::main]
async fn main() {
    let ctx = Context::new();

    let ok = plugin("greeter", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("greeting", "hello".to_string()).await?;
            Ok(())
        })
    });
    ctx.plugin(ok, None)
        .expect("spawn greeter")
        .join()
        .await
        .expect("greeter active");

    let failing = plugin("broken", vec![], |_ctx, _config| {
        Box::pin(async { Err(dougong::Error::msg("demo failure")) })
    });
    let broken = ctx.plugin(failing, None).expect("spawn broken");
    let _ = broken.join().await;

    let health = ctx.health();
    let body = json!({
        "healthy": health.healthy,
        "plugins": health.plugin_count,
        "fibers": {
            "live": health.live_fibers,
            "active": health.active_fibers,
            "failed": health.failed_fibers,
        },
        "failed_fibers": health
            .failed
            .iter()
            .map(|f| json!({"fiber": f.name, "uid": f.uid, "trace": f.trace, "error": f.error}))
            .collect::<Vec<_>>(),
        "recent_errors": health
            .recent_errors
            .iter()
            .map(|e| {
                let secs = e
                    .at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |age| age.as_secs());
                json!({"class": e.class.as_str(), "event": e.event, "fiber": e.fiber,
                       "trace": e.trace, "at_unix": secs})
            })
            .collect::<Vec<_>>(),
        "long_tasks": health
            .long_tasks
            .iter()
            .map(|t| json!({"fiber": t.name, "trace": t.trace,
                            "longest_ms": u64::try_from(t.longest.as_millis()).unwrap_or(u64::MAX)}))
            .collect::<Vec<_>>(),
        "stale_providers": health.stale_providers.len(),
    });
    ctx.logger()
        .log(LogLevel::Info, "health", &format!("healthz body: {body}"));
}
