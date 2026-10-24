// Rust guideline compliant 2026-10-24
//! Error classification and per-class policies (stability track S5, design
//! D20): every error carries a `logic` / `timeout` / `transport` class; the
//! host-graded policy table decides the log level, the listener-failure
//! reaction on the offending fiber, and the provider-quarantine isolation.
//! Defaults reproduce the pre-S5 behavior exactly.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{
    ApplyPolicy, ApplyTimeout, BoxFuture, ClassPolicy, Context, ContextOptions, Error, ErrorClass,
    ErrorPolicies, FiberHandle, FiberState, Invoker, ListenerFailurePolicy, LogLevel, Logger,
    LoggerHandle, ProviderQuarantine, Result, TimeoutConfig, plugin,
};
use serde_json::{Value, json};

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: Duration = Duration::from_secs(5);

/// Small real-time budget: deadline semantics only need the deadline to fire
/// eventually, so a short real duration is deterministic under load.
const BUDGET: Duration = Duration::from_millis(50);

/// Polls the condition with a hard ceiling: a hang fails, never flakes.
async fn wait_until(condition: impl Fn() -> bool + Send + Sync) {
    tokio::time::timeout(RAIL, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition reached");
}

/// A log sink capturing level and message of every record.
#[derive(Clone, Default)]
struct Capture {
    lines: Arc<Mutex<Vec<(LogLevel, String)>>>,
}

impl Capture {
    fn has(&self, level: LogLevel, needle: &str) -> bool {
        self.lines
            .lock()
            .expect("capture lock")
            .iter()
            .any(|(seen, line)| *seen == level && line.contains(needle))
    }
}

impl Logger for Capture {
    fn log(&self, level: LogLevel, _target: &str, message: &str) {
        self.lines
            .lock()
            .expect("capture lock")
            .push((level, message.to_string()));
    }
}

/// Installs `capture` as the `logger` service inside a fresh isolation label
/// and returns the isolated context the test must use for everything (the
/// kernel seeds the built-in stderr logger on the default label, so a global
/// replacement is impossible by design; isolation is the override mechanism).
async fn install_logger(ctx: &Context, capture: &Capture) -> Context {
    let isolated = ctx.isolate("logger");
    let capture = capture.clone();
    let logger_plugin = plugin("test-logger", vec![], move |ctx, _config| {
        let capture = capture.clone();
        Box::pin(async move {
            ctx.provide("logger", LoggerHandle::new(Arc::new(capture)))
                .await?;
            Ok(())
        })
    });
    isolated
        .plugin(logger_plugin, None)
        .expect("spawn logger plugin")
        .join()
        .await
        .expect("logger plugin active");
    isolated
}

/// A plugin whose every listener invocation on `event` fails with `err`.
fn failing_listener_plugin(
    name: &'static str,
    event: &'static str,
    err: fn() -> Error,
) -> Arc<dyn dougong::Plugin> {
    plugin(name, vec![], move |ctx, _config| {
        Box::pin(async move {
            let listener: dougong::Listener = Arc::new(move |_ctx, _payload, _next| {
                Box::pin(async move { Err(err()) }) as BoxFuture<Result<Value>>
            });
            ctx.on(event, listener).await?;
            Ok(())
        })
    })
}

#[test]
fn constructors_carry_their_class() {
    assert_eq!(Error::new("x", "m").class, ErrorClass::Logic);
    assert_eq!(Error::msg("m").class, ErrorClass::Logic);
    assert_eq!(
        Error::transport("ffi_status", "m").class,
        ErrorClass::Transport
    );
    assert_eq!(Error::apply_timeout("p").class, ErrorClass::Timeout);
    assert_eq!(Error::listener_timeout("e").class, ErrorClass::Timeout);
    assert_eq!(Error::join_timeout("p").class, ErrorClass::Timeout);
    assert_eq!(Error::plugin_panicked("x").class, ErrorClass::Logic);
    assert_eq!(Error::plugin_cancelled().class, ErrorClass::Logic);
    assert_eq!(Error::listener_panicked("x").class, ErrorClass::Logic);
    // The aggregate takes the most severe member class.
    let mixed = Error::aggregate(&[
        Error::new("x", "m"),
        Error::listener_timeout("e"),
        Error::transport("wasm_trap", "m"),
    ]);
    assert_eq!(mixed.class, ErrorClass::Transport);
    let mild = Error::aggregate(&[Error::new("x", "m"), Error::listener_timeout("e")]);
    assert_eq!(mild.class, ErrorClass::Timeout);
}

#[tokio::test]
async fn log_level_follows_the_error_class() {
    let capture = Capture::default();
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig {
            apply: Some(ApplyTimeout {
                budget: BUDGET,
                policy: ApplyPolicy::Fail,
            }),
            ..TimeoutConfig::default()
        },
        policies: ErrorPolicies {
            logic: ClassPolicy {
                log_level: LogLevel::Warn,
                ..ClassPolicy::default()
            },
            timeout: ClassPolicy {
                log_level: LogLevel::Debug,
                ..ClassPolicy::default()
            },
            transport: ClassPolicy {
                log_level: LogLevel::Info,
                ..ClassPolicy::default()
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let ctx = install_logger(&ctx, &capture).await;

    // Logic: a body failure parks the fiber `Failed` and logs at Warn.
    let bad = plugin("bad-body", vec![], |_ctx, _config| {
        Box::pin(async { Err(Error::new("boom", "body failed")) })
    });
    let handle = ctx.plugin(bad, None).expect("spawn bad-body");
    let _ = handle.join().await;
    wait_until(|| capture.has(LogLevel::Warn, "boom")).await;

    // Timeout: the apply budget fires and the failure logs at Debug.
    let hung = plugin("hung-body", vec![], |_ctx, _config| {
        Box::pin(async { std::future::pending().await })
    });
    let handle = ctx.plugin(hung, None).expect("spawn hung-body");
    let _ = handle.join().await;
    wait_until(|| capture.has(LogLevel::Debug, "apply_timeout")).await;

    // Transport: a fire-and-forget listener failing with a transport-class
    // error logs at Info.
    let listener = failing_listener_plugin("trap-listener", "boom", || {
        Error::transport("wasm_trap", "simulated trap")
    });
    ctx.plugin(listener, None)
        .expect("spawn trap-listener")
        .join()
        .await
        .expect("listener active");
    ctx.emit("boom", json!(null));
    wait_until(|| capture.has(LogLevel::Info, "wasm_trap")).await;
}

#[tokio::test]
async fn listener_failure_is_log_only_by_default() {
    let capture = Capture::default();
    let ctx = install_logger(&Context::new(), &capture).await;
    let handle = ctx
        .plugin(
            failing_listener_plugin("quiet-failure", "boom", || Error::new("boom", "nope")),
            None,
        )
        .expect("spawn listener");
    handle.join().await.expect("listener active");
    ctx.emit("boom", json!(null));
    wait_until(|| capture.has(LogLevel::Error, "boom")).await;
    // LogOnly: the fiber is left alone.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(handle.state(), FiberState::Active);
}

#[tokio::test]
async fn listener_failure_can_restart_the_offender() {
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig::default(),
        policies: ErrorPolicies {
            logic: ClassPolicy {
                on_listener_failure: ListenerFailurePolicy::RestartFiber,
                ..ClassPolicy::default()
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let bodies = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&bodies);
    let flaky = plugin("flaky", vec![], move |ctx, _config| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let listener: dougong::Listener = Arc::new(move |_ctx, _payload, _next| {
                Box::pin(async move { Err(Error::new("boom", "listener failed")) })
                    as BoxFuture<Result<Value>>
            });
            ctx.on("boom", listener).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(flaky, None).expect("spawn flaky");
    handle.join().await.expect("first activation");
    ctx.emit("boom", json!(null));
    wait_until(|| bodies.load(Ordering::SeqCst) >= 2).await;
    wait_until(|| handle.state() == FiberState::Active).await;
}

#[tokio::test]
async fn listener_failure_can_dispose_the_offender() {
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig::default(),
        policies: ErrorPolicies {
            logic: ClassPolicy {
                on_listener_failure: ListenerFailurePolicy::DisposeFiber,
                ..ClassPolicy::default()
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let handle = ctx
        .plugin(
            failing_listener_plugin("doomed", "boom", || Error::new("boom", "nope")),
            None,
        )
        .expect("spawn doomed");
    handle.join().await.expect("listener active");
    ctx.emit("boom", json!(null));
    wait_until(|| handle.state() == FiberState::Disposed).await;
}

#[tokio::test]
async fn parallel_still_aggregates_while_the_policy_reacts() {
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig::default(),
        policies: ErrorPolicies {
            logic: ClassPolicy {
                on_listener_failure: ListenerFailurePolicy::DisposeFiber,
                ..ClassPolicy::default()
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let handle = ctx
        .plugin(
            failing_listener_plugin("parallel-doomed", "ev", || Error::new("boom", "nope")),
            None,
        )
        .expect("spawn failing listener");
    let ok = plugin("parallel-ok", vec![], |ctx, _config| {
        Box::pin(async move {
            let listener: dougong::Listener = Arc::new(move |_ctx, _payload, _next| {
                Box::pin(async move { Ok(json!("fine")) }) as BoxFuture<Result<Value>>
            });
            ctx.on("ev", listener).await?;
            Ok(())
        })
    });
    ctx.plugin(ok, None).expect("spawn ok listener");
    handle.join().await.expect("failing listener active");
    // Propagation is the mode's contract and must survive the reaction.
    let err = ctx
        .parallel("ev", json!(null))
        .await
        .expect_err("one listener failed");
    assert_eq!(err.code, "aggregate");
    wait_until(|| handle.state() == FiberState::Disposed).await;
}

#[tokio::test]
async fn serial_propagates_and_restarts_the_offender() {
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig::default(),
        policies: ErrorPolicies {
            logic: ClassPolicy {
                on_listener_failure: ListenerFailurePolicy::RestartFiber,
                ..ClassPolicy::default()
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let bodies = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&bodies);
    let flaky = plugin("serial-flaky", vec![], move |ctx, _config| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let listener: dougong::Listener = Arc::new(move |_ctx, _payload, _next| {
                Box::pin(async move { Err(Error::new("boom", "serial failure")) })
                    as BoxFuture<Result<Value>>
            });
            ctx.on("ev", listener).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(flaky, None).expect("spawn flaky");
    handle.join().await.expect("first activation");
    let err = ctx.serial("ev", json!(null)).await.expect_err("must fail");
    assert_eq!(err.code, "boom");
    wait_until(|| bodies.load(Ordering::SeqCst) >= 2).await;
    wait_until(|| handle.state() == FiberState::Active).await;
}

/// A dynamic-channel provider whose calls fail with the factory's error.
struct FailingInvoke {
    err: fn() -> Error,
}

impl dougong::Invoke for FailingInvoke {
    fn call(&self, _method: u32, _payload: Vec<u8>) -> BoxFuture<Result<Vec<u8>>> {
        let err = (self.err)();
        Box::pin(async move { Err(err) })
    }
}

/// A plugin providing the `svc` dynamic channel backed by `FailingInvoke`.
fn failing_provider(name: &'static str, err: fn() -> Error) -> Arc<dyn dougong::Plugin> {
    plugin(name, vec![], move |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc", Invoker::new(Arc::new(FailingInvoke { err })))
                .await?;
            Ok(())
        })
    })
}

#[tokio::test]
async fn quarantine_disposes_a_provider_on_transport_failure() {
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig::default(),
        policies: ErrorPolicies {
            provider_quarantine: ProviderQuarantine::DisposeOnTransportError,
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let provider = ctx
        .plugin(
            failing_provider("broken-provider", || Error::transport("wasm_trap", "trap")),
            None,
        )
        .expect("spawn provider");
    provider.join().await.expect("provider active");
    let invoker = ctx.get::<Invoker>("svc").expect("service resolved");
    let err = invoker
        .call(0, Vec::new())
        .await
        .expect_err("transport fails");
    // The caller sees the failure unaltered; the quarantine acts out of band.
    assert_eq!(err.code, "wasm_trap");
    assert_eq!(err.class, ErrorClass::Transport);
    wait_until(|| provider.state() == FiberState::Disposed).await;
}

#[tokio::test]
async fn quarantine_ignores_logic_failures() {
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig::default(),
        policies: ErrorPolicies {
            provider_quarantine: ProviderQuarantine::DisposeOnTransportError,
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let provider = ctx
        .plugin(
            failing_provider("logic-provider", || {
                Error::new("service_error", "bad input")
            }),
            None,
        )
        .expect("spawn provider");
    provider.join().await.expect("provider active");
    let invoker = ctx.get::<Invoker>("svc").expect("service resolved");
    let err = invoker.call(0, Vec::new()).await.expect_err("logic fails");
    assert_eq!(err.class, ErrorClass::Logic);
    // A business failure is the caller's problem, never a quarantine reason.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(provider.state(), FiberState::Active);
}

#[tokio::test]
async fn quarantine_is_disabled_by_default() {
    let ctx = Context::new();
    let provider = ctx
        .plugin(
            failing_provider("default-provider", || Error::transport("wasm_trap", "trap")),
            None,
        )
        .expect("spawn provider");
    provider.join().await.expect("provider active");
    let invoker = ctx.get::<Invoker>("svc").expect("service resolved");
    let _ = invoker.call(0, Vec::new()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(provider.state(), FiberState::Active);
}

#[tokio::test]
async fn the_root_fiber_is_never_a_reaction_target() {
    let capture = Capture::default();
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig::default(),
        policies: ErrorPolicies {
            logic: ClassPolicy {
                on_listener_failure: ListenerFailurePolicy::DisposeFiber,
                ..ClassPolicy::default()
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let ctx = install_logger(&ctx, &capture).await;
    // A listener registered directly on the (root-fiber) context: the uid-0
    // guard must keep the framework fiber out of every reaction.
    let listener: dougong::Listener = Arc::new(move |_ctx, _payload, _next| {
        Box::pin(async move { Err(Error::new("boom", "root listener failed")) })
            as BoxFuture<Result<Value>>
    });
    ctx.on("root-boom", listener).await.expect("root listener");
    ctx.emit("root-boom", json!(null));
    wait_until(|| capture.has(LogLevel::Error, "root listener failed")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The runtime still works: a fresh plugin activates fine.
    let probe = plugin("probe", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle: FiberHandle = ctx.plugin(probe, None).expect("spawn probe");
    handle.join().await.expect("probe active");
    assert_eq!(handle.state(), FiberState::Active);
}
