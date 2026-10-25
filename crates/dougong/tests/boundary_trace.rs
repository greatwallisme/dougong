// Rust guideline compliant 2026-10-25
//! Boundary-call tracing (observability track O7, improve 三.7): with
//! `ContextOptions::boundary_trace`, every dynamic-channel call emits a
//! `boundary_call` record — service, method, payload size, duration,
//! outcome, and the provider identity (name/uid/trace/realm/boundary tag) —
//! plus always-on counters. Off by default: no wrapper, no records. The S5
//! quarantine reaction composes unchanged on top.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{
    Context, ContextOptions, ErrorCategory, ErrorPolicies, Invoker, LogLevel, LogRecord, Logger,
    LoggerHandle, ProviderQuarantine, plugin,
};

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

/// Spawns a provider exposing `remote` through the dynamic channel.
async fn provide_remote(ctx: &Context, invoke: Arc<dyn dougong::Invoke>) -> dougong::FiberHandle {
    let provider = plugin("remote-provider", vec![], move |ctx, _config| {
        let invoke = Arc::clone(&invoke);
        Box::pin(async move {
            ctx.provide("remote", Invoker::new(invoke)).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(provider, None).expect("spawn provider");
    handle.join().await.expect("provider active");
    handle
}

#[tokio::test]
async fn tracing_off_emits_no_boundary_records() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, sink.clone()).await;
    provide_remote(
        &isolated,
        Arc::new(OkInvoke {
            delay: Duration::from_millis(1),
        }),
    )
    .await;
    let remote = isolated.get::<Invoker>("remote").expect("resolved");
    remote.call(3, vec![1, 2, 3]).await.expect("call ok");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        sink.of("boundary_call").is_empty(),
        "tracing off means no wrapper and no records"
    );
    isolated.stop().await;
}

#[tokio::test]
async fn tracing_on_records_every_call_with_identity() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        boundary_trace: true,
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, sink.clone()).await;
    let provider_handle = provide_remote(
        &isolated,
        Arc::new(OkInvoke {
            delay: Duration::from_millis(5),
        }),
    )
    .await;
    let uid = provider_handle.uid().expect("live uid");

    let remote = isolated.get::<Invoker>("remote").expect("resolved");
    remote.call(7, vec![0_u8; 128]).await.expect("call ok");
    wait_until(|| !sink.of("boundary_call").is_empty()).await;

    let record = sink.of("boundary_call").pop().expect("boundary record");
    assert_eq!(record.level, LogLevel::Info);
    assert_eq!(record.target, "boundary");
    assert_eq!(record.service.as_deref(), Some("remote"));
    assert_eq!(record.fiber.as_deref(), Some("remote-provider"));
    assert_eq!(record.uid, Some(uid));
    assert_eq!(record.trace, provider_handle.uid());
    assert!(
        record
            .duration
            .is_some_and(|d| d >= Duration::from_millis(5)),
        "call duration"
    );
    assert!(
        record.message.contains("remote#7") && record.message.contains("bytes=128 ok"),
        "method and payload size ride the message: {}",
        record.message
    );
    assert_eq!(
        isolated.metrics().boundary_calls,
        1,
        "the O7 counter observed the call"
    );
    isolated.stop().await;
}

/// Boundary invoke that always fails transport-class.
#[derive(Debug)]
struct BrokenInvoke;

impl dougong::Invoke for BrokenInvoke {
    fn call(
        &self,
        _method: u32,
        _payload: Vec<u8>,
    ) -> dougong::BoxFuture<dougong::Result<Vec<u8>>> {
        Box::pin(async { Err(dougong::Error::transport("ffi_status", "broken")) })
    }
}

#[tokio::test]
async fn failures_are_graded_and_quarantine_composes() {
    let sink = RecordSink::default();
    let ctx = Context::new_with_options(ContextOptions {
        boundary_trace: true,
        policies: ErrorPolicies {
            provider_quarantine: ProviderQuarantine::DisposeOnTransportError,
            ..ErrorPolicies::default()
        },
        ..ContextOptions::default()
    });
    let isolated = isolate_with(&ctx, sink.clone()).await;
    let provider_handle = provide_remote(&isolated, Arc::new(BrokenInvoke)).await;
    let uid = provider_handle.uid().expect("live uid");

    let remote = isolated.get::<Invoker>("remote").expect("resolved");
    let _outcome = remote.call(1, Vec::new()).await.unwrap_err();

    wait_until(|| !sink.of("boundary_call").is_empty()).await;
    let record = sink.of("boundary_call").pop().expect("failure record");
    assert_eq!(record.level, LogLevel::Error, "graded by the S5 table");
    assert_eq!(record.class, Some(dougong::ErrorClass::Transport));
    assert_eq!(record.category, Some(ErrorCategory::Boundary));
    assert!(record.message.contains("failed"));
    assert!(
        record
            .cause
            .as_deref()
            .is_some_and(|c| c.contains("broken"))
    );
    assert!(
        !sink.of("provider_quarantine").is_empty(),
        "quarantine composes: the dispose reaction record fires"
    );
    wait_until(|| {
        isolated
            .health()
            .stale_providers
            .iter()
            .any(|stale| stale.uid == uid)
            || isolated.metrics().boundary_call_failures == 1
    })
    .await;
    assert_eq!(
        isolated.metrics().boundary_call_failures,
        1,
        "the failure counter observed the call"
    );
    isolated.stop().await;
}

/// A slow in-process invoke used by the off-switch test.
#[derive(Debug)]
struct OkInvoke {
    delay: Duration,
}

impl dougong::Invoke for OkInvoke {
    fn call(
        &self,
        _method: u32,
        _payload: Vec<u8>,
    ) -> dougong::BoxFuture<dougong::Result<Vec<u8>>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(Vec::new())
        })
    }
}
