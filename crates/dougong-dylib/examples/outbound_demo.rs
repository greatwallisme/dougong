// Rust guideline compliant 2026-10-24
//! ABI v3 outbound acceptance demo: the greeter dylib's own worker thread
//! performs a synchronous cross-plugin `require` and publishes an event
//! (the full chain), then two embedded arms prove the deterministic failure
//! modes of the vtable contract:
//!
//! - permission: requiring a service absent from the descriptor `inject`
//!   list fails with `STATUS_UNDECLARED` (capability error);
//! - reentrancy: requiring a service that calls back into greeter on the
//!   same thread fails with `STATUS_REENTRANCY` instead of deadlocking the
//!   per-instance lock.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, Error, Invoke, Invoker, LogLevel, plugin};
use dougong_abi::{STATUS_REENTRANCY, STATUS_UNDECLARED};
use dougong_dylib::load;
use greeter_plugin::{GREETER_HELLO, GREETER_REQUIRE, GREETER_WORKER_REPLY};
use serde_json::Value;

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

/// The declared dependency: echoes `oracle:<payload>`, except that the
/// payload `loop` calls back into greeter's own `greeting` service. That
/// callback is what closes the A -> oracle -> A reentrancy cycle whenever
/// the caller sits inside a greeter export (the `GREETER_REQUIRE` handler).
struct Oracle {
    ctx: Context,
}

impl Invoke for Oracle {
    fn call(&self, _method: u32, payload: Vec<u8>) -> dougong::BoxFuture<dougong::Result<Vec<u8>>> {
        if payload.as_slice() == b"loop" {
            let Some(invoker) = self.ctx.get::<Invoker>("greeting") else {
                return Box::pin(async { Err(Error::msg("greeting is not committed")) });
            };
            return invoker.call(GREETER_HELLO, b"loopback".to_vec());
        }
        Box::pin(async move { Ok([b"oracle:".as_ref(), &payload].concat()) })
    }
}

/// Unpacks the `i32 little-endian status || payload` framing that
/// [`GREETER_REQUIRE`] documents.
fn unpack_framed(bytes: &[u8]) -> dougong::Result<(i32, &[u8])> {
    if bytes.len() < 4 {
        return Err(Error::msg("framed reply shorter than the status prefix"));
    }
    let (head, payload) = bytes.split_at(4);
    let head: [u8; 4] = head
        .try_into()
        .map_err(|err| Error::msg(format!("framing prefix is not four bytes: {err}")))?;
    Ok((i32::from_le_bytes(head), payload))
}

/// Parses the JSON detail document a failure status carries.
fn detail_code(payload: &[u8]) -> dougong::Result<String> {
    let detail: Value = serde_json::from_slice(payload)
        .map_err(|err| Error::msg(format!("detail is not JSON: {err}")))?;
    detail
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::msg("detail document lacks a \"code\" field"))
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
            logger.log(LogLevel::Info, "outbound-demo", &message);
        };

        log("-- loaded dylib plugin --".to_string());

        // The oracle must be committed before greeter activates: the
        // plugin's worker thread requires it from `setup` time onward.
        let oracle = plugin("oracle", vec![], |ctx, _config| {
            Box::pin(async move {
                let service = Oracle { ctx: ctx.clone() };
                ctx.provide("greeter.oracle", Invoker::new(Arc::new(service)))
                    .await?;
                Ok(())
            })
        });
        let oracle_fiber = ctx.plugin(oracle, None)?;
        oracle_fiber.join().await?;

        // Registered before greeter loads: the worker's `greeter/ready` emit
        // is fire-and-forget, so the listener must already be in place.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Value>();
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
                    Ok(Value::Null)
                })
            }),
        )
        .await?;

        let greeter_fiber = ctx.plugin(greeter, None)?;
        greeter_fiber.join().await?;

        // Full chain: the worker thread's emit arrived, and its synchronous
        // require reply is read back through greeter's own service.
        let ready = tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .map_err(|err| Error::msg(format!("greeter/ready did not arrive: {err}")))?
            .map_err(|err| Error::msg(format!("greeter/ready listener dropped: {err}")))?;
        assert_eq!(
            ready.get("source").and_then(Value::as_str),
            Some("greeter-worker")
        );
        let greeting = ctx
            .get::<Invoker>("greeting")
            .ok_or_else(|| Error::msg("greeting is not committed"))?;
        let reply = greeting.call(GREETER_WORKER_REPLY, Vec::new()).await?;
        assert_eq!(reply, b"oracle:worker-hello");
        log("-- full chain: own-thread require + emit verified --".to_string());

        // Permission arm: `unlisted` is not in greeter's descriptor `inject`
        // list, so the require is rejected before any resolution happens.
        let denied = greeting
            .call(GREETER_REQUIRE, b"unlisted\0probe".to_vec())
            .await?;
        let (status, detail) = unpack_framed(&denied)?;
        assert_eq!(status, STATUS_UNDECLARED);
        assert_eq!(detail_code(detail)?, "undeclared_dependency");
        log("-- permission arm: undeclared require rejected --".to_string());

        // Reentrancy arm: the `GREETER_REQUIRE` handler runs inside a greeter
        // export, so the oracle's `loop` callback into `greeting` would
        // re-enter the same instance on the same thread. The timeout is the
        // regression guard: a deadlock fails this demo instead of hanging it.
        let cyclic = tokio::time::timeout(
            Duration::from_secs(5),
            greeting.call(GREETER_REQUIRE, b"greeter.oracle\0loop".to_vec()),
        )
        .await
        .map_err(|err| Error::msg(format!("reentrancy arm deadlocked: {err}")))??;
        let (status, detail) = unpack_framed(&cyclic)?;
        assert_eq!(status, STATUS_REENTRANCY);
        assert_eq!(detail_code(detail)?, "reentrancy_cycle");
        log("-- reentrancy arm: cycle rejected deterministically --".to_string());

        greeter_fiber.dispose().await;
        ctx.stop().await;
        log("-- outbound demo done --".to_string());
        Ok::<(), Error>(())
    })?;
    Ok(())
}
