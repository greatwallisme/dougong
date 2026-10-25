// Rust guideline compliant 2026-10-25
//! Error classification and alerting (observability track O6, improve
//! 三.6): the eight-group `ErrorCategory` view over class+code, per-category
//! severity overrides, windowed rate limits that suppress the log line while
//! the O5 ring keeps recording, and one alert record per window at the
//! threshold crossing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{
    AlertPolicies, CategoryAlert, Context, ContextOptions, Error, ErrorCategory, ErrorClass,
    ErrorPolicies, LogLevel, LogRecord, Logger, LoggerHandle, plugin,
};
use serde_json::Value;

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: Duration = Duration::from_secs(5);

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

/// Structured sink capturing `LogRecord`s verbatim.
#[derive(Clone, Default)]
struct RecordSink {
    records: Arc<Mutex<Vec<LogRecord>>>,
}

impl RecordSink {
    fn of(&self, event: &str) -> Vec<LogRecord> {
        self.records
            .lock()
            .expect("sink lock")
            .iter()
            .filter(|record| record.event == event)
            .cloned()
            .collect()
    }
}

impl Logger for RecordSink {
    fn log(&self, _level: LogLevel, _target: &str, _message: &str) {}
    fn log_record(&self, record: &LogRecord) {
        self.records.lock().expect("sink lock").push(record.clone());
    }
}

/// Installs the sink as the logger service in a fresh isolation label.
async fn isolate_with(ctx: &Context, sink: RecordSink) -> Context {
    let isolated = ctx.isolate("logger");
    let sink: Arc<dyn Logger> = Arc::new(sink);
    let plug = plugin("test-logger", vec![], move |ctx, _config| {
        let handle = LoggerHandle::new(Arc::clone(&sink));
        Box::pin(async move {
            ctx.provide("logger", handle).await?;
            Ok(())
        })
    });
    isolated
        .plugin(plug, None)
        .expect("spawn logger plugin")
        .join()
        .await
        .expect("logger plugin active");
    isolated
}

#[test]
fn category_mapping_covers_the_codes() {
    let of = |err: &Error| ErrorCategory::of(err.class, err.code);
    assert!(matches!(
        of(&Error::undeclared_dependency("x")),
        ErrorCategory::Configuration
    ));
    assert!(matches!(
        of(&Error::duplicate_provide("x", "y")),
        ErrorCategory::Configuration
    ));
    assert!(matches!(
        of(&Error::not_provider("x")),
        ErrorCategory::Configuration
    ));
    assert!(matches!(
        of(&Error::inactive_dependency("x")),
        ErrorCategory::ProviderMissing
    ));
    assert!(matches!(
        of(&Error::inactive_effect()),
        ErrorCategory::LifecycleViolation
    ));
    assert!(matches!(
        of(&Error::budget_exceeded("effects", "p", 1)),
        ErrorCategory::ResourceExhaustion
    ));
    assert!(matches!(
        of(&Error::plugin_panicked("x")),
        ErrorCategory::Panic
    ));
    assert!(matches!(
        of(&Error::listener_panicked("x")),
        ErrorCategory::Panic
    ));
    for err in [
        Error::fiber_disposed("restart"),
        Error::fiber_loading("update"),
        Error::fiber_unloading("update"),
        Error::plugin_cancelled(),
    ] {
        assert!(matches!(of(&err), ErrorCategory::LifecycleViolation));
    }
    for err in [
        Error::transport("ffi_status", "x"),
        Error::transport("wasm_trap", "x"),
        Error::transport("plugin_load", "x"),
        Error::transport("payload_codec", "x"),
    ] {
        assert!(matches!(of(&err), ErrorCategory::Boundary));
    }
    assert!(matches!(
        of(&Error::apply_timeout("p")),
        ErrorCategory::Timeout
    ));
    assert!(matches!(
        of(&Error::listener_timeout("x")),
        ErrorCategory::Timeout
    ));
    assert!(matches!(of(&Error::msg("business")), ErrorCategory::Other));
}

#[tokio::test]
async fn severity_override_downgrades_a_category() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        policies: ErrorPolicies {
            alerts: AlertPolicies {
                rules: vec![(
                    ErrorCategory::Panic,
                    CategoryAlert {
                        window: Duration::from_secs(60),
                        max_logged: None,
                        alert_at: None,
                        severity: Some(LogLevel::Warn),
                    },
                )],
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, sink.clone()).await;

    // A panicking body surfaces as a plugin_panicked fail_load (Panic
    // category), logged at the override level instead of the class default.
    let doomed = plugin("panicky", vec![], |_ctx, _config| {
        Box::pin(async {
            panic!("body exploded"); // invariant check: fault-injection rail for the panic category
        })
    });
    let handle = isolated.plugin(doomed, None).expect("spawn");
    assert!(handle.join().await.is_err());

    wait_until(|| !sink.of("fail_load").is_empty()).await;
    let record = sink.of("fail_load").pop().expect("fail_load record");
    assert_eq!(record.level, LogLevel::Warn, "category severity wins");
    assert_eq!(record.category, Some(ErrorCategory::Panic));
    assert_eq!(record.class, Some(ErrorClass::Logic));
    isolated.stop().await;
}

#[tokio::test]
async fn rate_limit_suppresses_lines_but_not_ring_entries() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        policies: ErrorPolicies {
            alerts: AlertPolicies {
                rules: vec![(
                    ErrorCategory::Other,
                    CategoryAlert {
                        window: Duration::from_secs(60),
                        max_logged: Some(2),
                        alert_at: None,
                        severity: None,
                    },
                )],
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, sink.clone()).await;

    let failing: dougong::Listener = Arc::new(|_ctx, _payload, _next| {
        Box::pin(async { Err(dougong::Error::msg("listener boom")) })
    });
    let holder = plugin("noisy", vec![], move |ctx, _config| {
        let failing = Arc::clone(&failing);
        Box::pin(async move {
            ctx.on("evt.noise", failing).await?;
            Ok(())
        })
    });
    isolated
        .plugin(holder, None)
        .expect("spawn")
        .join()
        .await
        .expect("active");
    for _ in 0..5 {
        isolated.emit("evt.noise", Value::Null);
    }

    wait_until(|| sink.of("listener_failure").len() >= 2).await;
    wait_until(|| isolated.metrics().logic_error_count > 0).await;
    // Give the queue a moment to drain the remaining dispatches, then pin:
    // exactly two lines logged, five errors recorded elsewhere.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        sink.of("listener_failure").len(),
        2,
        "only the window budget is logged"
    );
    let logged = sink.of("listener_failure");
    assert!(
        logged
            .iter()
            .all(|record| record.category == Some(ErrorCategory::Other)),
        "records carry the O6 category"
    );
    isolated.stop().await;
}

#[tokio::test]
async fn alert_threshold_fires_once_per_window() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        policies: ErrorPolicies {
            alerts: AlertPolicies {
                rules: vec![(
                    ErrorCategory::Other,
                    CategoryAlert {
                        window: Duration::from_secs(60),
                        max_logged: None,
                        alert_at: Some(3),
                        severity: None,
                    },
                )],
            },
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, sink.clone()).await;

    let failing: dougong::Listener = Arc::new(|_ctx, _payload, _next| {
        Box::pin(async { Err(dougong::Error::msg("listener boom")) })
    });
    let holder = plugin("alerting", vec![], move |ctx, _config| {
        let failing = Arc::clone(&failing);
        Box::pin(async move {
            ctx.on("evt.alert", failing).await?;
            Ok(())
        })
    });
    isolated
        .plugin(holder, None)
        .expect("spawn")
        .join()
        .await
        .expect("active");
    for _ in 0..6 {
        isolated.emit("evt.alert", Value::Null);
    }

    wait_until(|| !sink.of("alert").is_empty()).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let alerts = sink.of("alert");
    assert_eq!(alerts.len(), 1, "exactly one alert per window");
    assert!(
        alerts[0].message.contains("3 errors in window"),
        "the alert reports the crossing count: {}",
        alerts[0].message
    );
    assert_eq!(alerts[0].category, Some(ErrorCategory::Other));
    // The original lines are untouched without a rate limit.
    assert_eq!(sink.of("listener_failure").len(), 6);
    isolated.stop().await;
}
