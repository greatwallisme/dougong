// Rust guideline compliant 2026-10-25
//! Structured logging (observability track O1, improve 三.1): every kernel
//! log line is a `LogRecord` carrying the field table — plugin/fiber name,
//! uid, realm, event, service, mode, old/new state, cause, timestamp,
//! duration — and sinks that only implement the flat `Logger::log` still
//! receive every record verbatim through the default degradation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{
    BudgetConfig, Context, ContextOptions, DispatchMode, Disposer, DrainPolicy, DrainTimeout,
    ErrorClass, ErrorPolicies, FiberState, Invoker, Listener, LogLevel, LogRecord, Logger,
    LoggerHandle, ProviderQuarantine, TimeoutConfig, plugin,
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

/// A fully structured sink: captures `LogRecord`s verbatim.
#[derive(Clone, Default)]
struct RecordSink {
    records: Arc<Mutex<Vec<LogRecord>>>,
}

impl RecordSink {
    fn find(&self, event: &str) -> Option<LogRecord> {
        self.records
            .lock()
            .expect("sink lock")
            .iter()
            .find(|record| record.event == event)
            .cloned()
    }

    fn transitions(&self) -> Vec<(FiberState, FiberState)> {
        self.records
            .lock()
            .expect("sink lock")
            .iter()
            .filter(|record| record.event == "state_transition")
            .filter_map(|record| Some((record.from_state?, record.to_state?)))
            .collect()
    }
}

impl Logger for RecordSink {
    fn log(&self, _level: LogLevel, _target: &str, _message: &str) {
        // The structured override catches everything; this stays empty so a
        // record that skips `log_record` shows up as a missing record.
    }

    fn log_record(&self, record: &LogRecord) {
        self.records.lock().expect("sink lock").push(record.clone());
    }
}

/// A pre-O1 style sink: implements only the flat face.
#[derive(Clone, Default)]
struct FlatCapture {
    lines: Arc<Mutex<Vec<(LogLevel, String)>>>,
}

impl FlatCapture {
    fn has(&self, level: LogLevel, needle: &str) -> bool {
        self.lines
            .lock()
            .expect("capture lock")
            .iter()
            .any(|(seen, line)| *seen == level && line.contains(needle))
    }
}

impl Logger for FlatCapture {
    fn log(&self, level: LogLevel, _target: &str, message: &str) {
        self.lines
            .lock()
            .expect("capture lock")
            .push((level, message.to_string()));
    }
}

/// Installs `logger` as the `logger` service in a fresh isolation label and
/// returns the isolated context the test must use for everything.
async fn isolate_with(ctx: &Context, logger: Arc<dyn Logger>) -> Context {
    let isolated = ctx.isolate("logger");
    let plug = plugin("test-logger", vec![], move |ctx, _config| {
        let handle = LoggerHandle::new(Arc::clone(&logger));
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

#[tokio::test]
async fn cleanup_timeout_record_carries_the_field_table() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig {
            cleanup: Some(Duration::from_millis(20)),
            ..TimeoutConfig::default()
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let orphan = plugin("orphan", vec![], |ctx, _config| {
        Box::pin(async move {
            // Orphanable teardown (the public effect default): the cleanup
            // budget applies, and the hung disposer orphans on breach.
            ctx.effect("stuck", async {
                let disposer: Disposer =
                    Box::new(|| Box::pin(tokio::time::sleep(Duration::from_secs(30))));
                Ok(Some(disposer))
            })
            .await?;
            Ok(())
        })
    });
    let handle = isolated.plugin(orphan, None).expect("spawn orphan");
    handle.join().await.expect("orphan active");
    // Unload via restart: the uid stays live during teardown, so the record
    // carries it (dispose claims the uid before draining by S4 design — a
    // dispose-driven cleanup record legitimately has no uid).
    handle
        .restart()
        .await
        .expect("restart completes despite the orphan");

    wait_until(|| sink.find("cleanup_timeout").is_some()).await;
    let record = sink
        .find("cleanup_timeout")
        .expect("cleanup_timeout record");
    assert_eq!(record.target, "fiber");
    assert_eq!(record.fiber.as_deref(), Some("orphan"));
    assert!(record.uid.is_some(), "fiber uid must be present");
    assert_eq!(record.service.as_deref(), Some("stuck"));
    assert_eq!(record.duration, Some(Duration::from_millis(20)));
    assert!(record.timestamp <= std::time::SystemTime::now());
    // The class is stamped and the level graded by the S5 policy table
    // (the default Timeout policy logs at Error).
    assert_eq!(record.level, LogLevel::Error);
    assert_eq!(record.class, Some(ErrorClass::Timeout));
}

#[tokio::test]
async fn lifecycle_transitions_log_from_and_to() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let plug = plugin("lifecycle", vec![], |_ctx, _config| {
        Box::pin(async { Ok(()) })
    });
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    assert_eq!(handle.state(), FiberState::Active);

    wait_until(|| !sink.transitions().is_empty()).await;
    let transitions = sink.transitions();
    // Activation must land at Active from a pre-activation state (Pending
    // or Loading — the mirror legitimately caches Loading while the body
    // runs). Reaching Pending again on stop is the mirror flip back.
    let activation = transitions
        .iter()
        .find(|(_, to)| *to == FiberState::Active)
        .copied()
        .expect("a transition ending at Active");
    assert!(
        matches!(activation.0, FiberState::Pending | FiberState::Loading),
        "activation must start from Pending or Loading, got {activation:?}"
    );
    isolated.stop().await;
}

#[tokio::test]
async fn transition_record_shape_is_ungraded_info_with_identity() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;
    let plug = plugin("shaped", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    wait_until(|| {
        sink.records
            .lock()
            .expect("sink lock")
            .iter()
            .any(|record| {
                record.event == "state_transition" && record.fiber.as_deref() == Some("shaped")
            })
    })
    .await;

    let record = sink
        .records
        .lock()
        .expect("sink lock")
        .iter()
        .find(|record| {
            record.event == "state_transition" && record.fiber.as_deref() == Some("shaped")
        })
        .cloned()
        .expect("transition record for the shaped plugin");
    assert_eq!(record.level, LogLevel::Info);
    assert_eq!(record.target, "fiber");
    assert!(record.uid.is_some());
    // Lifecycle lines are not error-class records: ungraded, no policy.
    assert_eq!(record.class, None);
    assert!(record.timestamp <= std::time::SystemTime::now());
    isolated.stop().await;
}

#[tokio::test]
async fn disposal_transitions_reach_disposed() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;
    let plug = plugin("mortal", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    handle.dispose().await;

    wait_until(|| {
        sink.transitions()
            .iter()
            .any(|(_, to)| *to == FiberState::Disposed)
    })
    .await;
    isolated.stop().await;
}

#[tokio::test]
async fn flat_only_sinks_receive_the_degraded_transition_line() {
    let flat = FlatCapture::default();
    let ctx = Context::new_with_options(ContextOptions {
        timeouts: TimeoutConfig {
            drain: Some(DrainTimeout {
                budget: Duration::from_millis(20),
                policy: DrainPolicy::LogAndContinue,
            }),
            ..TimeoutConfig::default()
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, Arc::new(flat.clone())).await;
    let plug = plugin("flat", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");

    // The default `log_record` degrades into the v1 `log` face, keeping the
    // graded level and the plain-text shape.
    wait_until(|| flat.has(LogLevel::Info, "state_transition")).await;
    isolated.stop().await;
}

/// Boundary invoke that always fails transport-class (the S5 quarantine
/// trigger class, which in-process code never produces on its own).
#[derive(Debug)]
struct Boundary;

impl dougong::Invoke for Boundary {
    fn call(
        &self,
        _method: u32,
        _payload: Vec<u8>,
    ) -> dougong::BoxFuture<dougong::Result<Vec<u8>>> {
        Box::pin(async { Err(dougong::Error::transport("ffi_status", "broken")) })
    }
}

#[tokio::test]
async fn quarantine_record_names_the_service() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        policies: ErrorPolicies {
            provider_quarantine: ProviderQuarantine::DisposeOnTransportError,
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let provider = plugin("bad-provider", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("remote", Invoker::new(Arc::new(Boundary)))
                .await?;
            Ok(())
        })
    });
    isolated
        .plugin(provider, None)
        .expect("spawn provider")
        .join()
        .await
        .expect("provider active");
    let remote = isolated.get::<Invoker>("remote").expect("remote resolved");

    let outcome = remote.call(1, Vec::new()).await;
    assert!(outcome.is_err(), "the boundary call must fail");
    wait_until(|| sink.find("provider_quarantine").is_some()).await;

    let record = sink.find("provider_quarantine").expect("quarantine record");
    assert_eq!(record.target, "policy");
    assert_eq!(record.service.as_deref(), Some("remote"));
    assert_eq!(record.fiber.as_deref(), Some("bad-provider"));
    assert!(record.uid.is_some(), "quarantine carries the provider uid");
    assert_eq!(record.level, LogLevel::Error);
    assert!(record.cause.is_some());
    isolated.stop().await;
}

#[tokio::test]
async fn emit_budget_breach_record_names_the_event() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        budgets: BudgetConfig {
            max_effects: None,
            max_event_emits: Some(1),
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let emitter = plugin("emitter", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.emit("evt.breach", Value::Null);
            ctx.emit("evt.breach", Value::Null);
            Ok(())
        })
    });
    isolated
        .plugin(emitter, None)
        .expect("spawn emitter")
        .join()
        .await
        .expect("emitter active");

    wait_until(|| sink.find("budget_breach").is_some()).await;
    let record = sink.find("budget_breach").expect("breach record");
    assert_eq!(record.event_name.as_deref(), Some("evt.breach"));
    assert_eq!(record.fiber.as_deref(), Some("emitter"));
    assert!(record.message.contains("event budget of 1 exceeded"));
    assert_eq!(record.class, Some(ErrorClass::Logic));
    isolated.stop().await;
}

#[tokio::test]
async fn listener_failure_record_carries_mode_and_event() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let failing: Listener = Arc::new(|_ctx, _payload, _next| {
        Box::pin(async { Err(dougong::Error::msg("listener exploded")) })
    });
    let holder = plugin("holder", vec![], move |ctx, _config| {
        let failing = Arc::clone(&failing);
        Box::pin(async move {
            ctx.on("evt.fail", failing).await?;
            Ok(())
        })
    });
    isolated
        .plugin(holder, None)
        .expect("spawn holder")
        .join()
        .await
        .expect("holder active");

    isolated.emit("evt.fail", Value::Null);
    wait_until(|| sink.find("listener_failure").is_some()).await;

    let record = sink.find("listener_failure").expect("failure record");
    assert_eq!(record.target, "events");
    assert_eq!(record.event_name.as_deref(), Some("evt.fail"));
    assert_eq!(record.mode, Some(DispatchMode::Emit));
    assert!(record.message.contains("listener exploded"));
    isolated.stop().await;
}

#[tokio::test]
async fn to_flat_preserves_the_message_and_appends_fields() {
    let record = LogRecord::new(
        LogLevel::Warn,
        "fiber",
        "cleanup_timeout",
        "disposer \"pool\" exceeded".to_string(),
    )
    .with_fiber("app".to_string(), Some(7))
    .with_states(FiberState::Active, FiberState::Unloading)
    .with_duration(Duration::from_millis(5));
    let flat = record.to_flat();
    assert!(
        flat.starts_with("cleanup_timeout: disposer \"pool\" exceeded"),
        "flat must keep the kind prefix plus verbatim message: {flat}"
    );
    assert!(flat.contains("fiber=\"app\""), "{flat}");
    assert!(flat.contains("uid=7"), "{flat}");
    assert!(flat.contains("state=active->unloading"), "{flat}");
    assert!(flat.contains("duration=5ms"), "{flat}");
}

/// Pins that a record without context fields renders as the bare kind and
/// message — the pre-O1 plain-text surface for prose-only sites.
#[tokio::test]
async fn bare_record_flat_has_no_field_block() {
    let record = LogRecord::new(LogLevel::Error, "dougong", "error", "boom".to_string());
    assert_eq!(record.to_flat(), "error: boom");
    let _ = AtomicBool::new(false).load(Ordering::Relaxed);
}
