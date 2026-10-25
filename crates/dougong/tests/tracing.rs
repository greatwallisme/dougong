// Rust guideline compliant 2026-10-25
//! Per-fiber trace ids and lifecycle spans (observability track O3, improve
//! 三.3): every fiber carries a trace id — its uid as captured at
//! construction, stable across dispose — every kernel record of that fiber
//! carries the trace plus the plugin form's boundary tag, state-transition
//! records carry the exited state's span duration, and the `internal/status`
//! payload names the trace.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{
    BoxFuture, Context, FiberState, LogLevel, LogRecord, Logger, LoggerHandle, Plugin, plugin,
};
use serde_json::{Value, json};

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
    fn for_fiber(&self, fiber: &str, event: &str) -> Vec<LogRecord> {
        self.records
            .lock()
            .expect("sink lock")
            .iter()
            .filter(|record| record.event == event && record.fiber.as_deref() == Some(fiber))
            .cloned()
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

/// A manual plugin declaring a synthetic boundary tag: pins that the kernel
/// stamps whatever `Plugin::boundary_tag` reports onto the fiber's records.
struct Tagged {
    name: &'static str,
    tag: Option<&'static str>,
}

impl Plugin for Tagged {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(self.name)
    }

    fn boundary_tag(&self) -> Option<&'static str> {
        self.tag
    }

    fn apply(&self, _ctx: Context, _config: Value) -> BoxFuture<dougong::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn root_traces_zero_and_fibers_trace_their_uid() {
    let ctx = Context::new();
    assert_eq!(ctx.trace_id(), 0, "the root context traces as the sentinel");

    let sink = RecordSink::default();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let seen = Arc::new(Mutex::new(None::<u64>));
    let body_seen = Arc::clone(&seen);
    let plug = plugin("traced", vec![], move |ctx, _config| {
        let body_seen = Arc::clone(&body_seen);
        Box::pin(async move {
            *body_seen.lock().expect("seen lock") = Some(ctx.trace_id());
            Ok(())
        })
    });
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");

    let trace = seen.lock().expect("seen lock").expect("body ran");
    assert!(trace > 0, "child fibers never trace as the root sentinel");
    assert_eq!(
        handle.uid(),
        Some(trace),
        "the trace id is the uid captured at construction"
    );

    wait_until(|| !sink.for_fiber("traced", "state_transition").is_empty()).await;
    for record in sink.for_fiber("traced", "state_transition") {
        assert_eq!(
            record.trace,
            Some(trace),
            "every lifecycle record of the fiber carries the same trace"
        );
    }
    isolated.stop().await;
}

#[tokio::test]
async fn transition_records_carry_span_duration_and_no_boundary_in_process() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let plug = plugin("spanned", vec![], |_ctx, _config| {
        Box::pin(async { Ok(()) })
    });
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");

    wait_until(|| !sink.for_fiber("spanned", "state_transition").is_empty()).await;
    let record = sink
        .for_fiber("spanned", "state_transition")
        .pop()
        .expect("transition record");
    assert_eq!(
        record.trace,
        handle.uid(),
        "transition records carry the fiber trace"
    );
    assert!(
        record.duration.is_some(),
        "a transition record carries the exited state's span duration"
    );
    assert_eq!(
        record.boundary, None,
        "in-process plugins have no boundary tag"
    );
    isolated.stop().await;
}

#[tokio::test]
async fn declared_boundary_tag_reaches_the_records() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let tagged: Arc<dyn Plugin> = Arc::new(Tagged {
        name: "tagged",
        tag: Some("synthetic"),
    });
    let spawned = isolated.plugin(tagged, None).expect("spawn tagged");
    spawned.join().await.expect("tagged active");
    let handle = spawned;

    wait_until(|| !sink.for_fiber("tagged", "state_transition").is_empty()).await;
    for record in sink.for_fiber("tagged", "state_transition") {
        assert_eq!(
            record.boundary,
            Some("synthetic"),
            "the kernel stamps Plugin::boundary_tag onto the fiber's records"
        );
        assert_eq!(record.trace, Some(handle.uid().expect("live uid")));
    }
    isolated.stop().await;
}

#[tokio::test]
async fn trace_survives_dispose_while_uid_is_retracted() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let plug = plugin("mortal", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    let trace = handle.uid().expect("live uid");
    handle.dispose().await;

    wait_until(|| {
        sink.for_fiber("mortal", "state_transition")
            .iter()
            .any(|record| record.to_state == Some(FiberState::Disposed))
    })
    .await;
    let disposed = sink
        .for_fiber("mortal", "state_transition")
        .into_iter()
        .find(|record| record.to_state == Some(FiberState::Disposed))
        .expect("disposed transition record");
    // S4 claims the uid before teardown, so the terminal record has no uid —
    // the trace is the join key that still names the fiber's whole chain.
    assert_eq!(disposed.uid, None, "dispose retracts the uid");
    assert_eq!(
        disposed.trace,
        Some(trace),
        "the trace survives dispose: teardown stays joinable to the lifecycle"
    );
    isolated.stop().await;
}

#[tokio::test]
async fn status_payload_names_the_trace() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let payloads: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_payloads = Arc::clone(&payloads);
    let listener: dougong::Listener = Arc::new(move |_ctx, payload, _next| {
        let sink_payloads = Arc::clone(&sink_payloads);
        Box::pin(async move {
            sink_payloads
                .lock()
                .expect("payload lock")
                .push(payload.clone());
            Ok(payload)
        })
    });
    isolated
        .on("internal/status", listener)
        .await
        .expect("register status listener");

    let plug = plugin("reported", vec![], |_ctx, _config| {
        Box::pin(async { Ok(()) })
    });
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    let trace = handle.uid().expect("live uid");

    wait_until(|| {
        payloads
            .lock()
            .expect("payload lock")
            .iter()
            .any(|payload| payload["fiber"] == json!("reported"))
    })
    .await;
    let seen = payloads
        .lock()
        .expect("payload lock")
        .iter()
        .find(|payload| payload["fiber"] == json!("reported"))
        .cloned()
        .expect("status payload for the reported fiber");
    assert_eq!(seen["uid"], json!(trace));
    assert_eq!(seen["trace"], json!(trace), "the payload names the trace");
    isolated.stop().await;
}

#[tokio::test]
async fn failing_activation_records_share_one_trace() {
    let sink = RecordSink::default();
    let ctx = Context::new();
    let isolated = isolate_with(&ctx, Arc::new(sink.clone())).await;

    let doomed = plugin("doomed", vec![], |_ctx, _config| {
        Box::pin(async { Err(dougong::Error::msg("boom")) })
    });
    let handle = isolated.plugin(doomed, None).expect("spawn");
    assert!(handle.join().await.is_err(), "activation must fail");

    wait_until(|| !sink.for_fiber("doomed", "fail_load").is_empty()).await;
    let failure = sink
        .for_fiber("doomed", "fail_load")
        .pop()
        .expect("fail_load record");
    let traces: Vec<Option<u64>> = sink
        .for_fiber("doomed", "state_transition")
        .iter()
        .map(|record| record.trace)
        .collect();
    assert!(
        !traces.is_empty() && traces.iter().all(|trace| *trace == failure.trace),
        "transitions and the failure record share one trace, got {traces:?} vs {:?}",
        failure.trace
    );
    assert!(
        failure.trace.is_some(),
        "the failure record carries the trace"
    );
    isolated.stop().await;
}
