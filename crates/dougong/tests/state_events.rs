// Rust guideline compliant 2026-10-25
//! System state events (observability track O4, improve 三.4): the named
//! `internal/*` family — fiber.created, fiber.<state>, provider.changed,
//! effect.cleaned, drain.started/finished — all carrying the common identity
//! envelope (fiber/uid/trace/realm). `service.notified` and
//! `event.dispatched` are served by the pre-existing `internal/service` and
//! `internal/dispatch` events (D27 mapping, no duplicate channel).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, Disposer, Listener, LogLevel, LogRecord, Logger, LoggerHandle, plugin};
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

/// Shared capture of `(event name, payload)` pairs from the system family.
#[derive(Clone, Default)]
struct EventLog {
    entries: Arc<Mutex<Vec<(String, Value)>>>,
}

impl EventLog {
    fn of(&self, name: &str) -> Vec<Value> {
        self.entries
            .lock()
            .expect("event log lock")
            .iter()
            .filter(|(seen, _)| seen == name)
            .map(|(_, payload)| payload.clone())
            .collect()
    }

    fn of_fiber(&self, name: &str, fiber: &str) -> Vec<Value> {
        self.of(name)
            .into_iter()
            .filter(|payload| payload["fiber"] == json!(fiber))
            .collect()
    }
}

/// A structured sink so the kernel's log lines stay out of the test output.
#[derive(Clone, Default)]
struct Quiet;

impl Logger for Quiet {
    fn log(&self, _level: LogLevel, _target: &str, _message: &str) {}
}

impl Logger for EventLog {
    fn log(&self, _level: LogLevel, _target: &str, _message: &str) {}
}

/// Registers a capture listener for `name` on the test context.
async fn capture(ctx: &Context, log: &EventLog, name: &str) {
    let sink = log.clone();
    let event_name = name.to_string();
    let listener: Listener = Arc::new(move |_ctx, payload, _next| {
        let sink = sink.clone();
        let event_name = event_name.clone();
        Box::pin(async move {
            sink.entries
                .lock()
                .expect("event log lock")
                .push((event_name, payload.clone()));
            Ok(payload)
        })
    });
    ctx.on(name, listener)
        .await
        .expect("register capture listener");
}

/// Fresh isolated context with the quiet logger and all O4 events captured.
async fn test_context(log: &EventLog) -> Context {
    let ctx = Context::new();
    let isolated = ctx.isolate("logger");
    let quiet = plugin("test-logger", vec![], move |ctx, _config| {
        let handle = LoggerHandle::new(Arc::new(Quiet));
        Box::pin(async move {
            ctx.provide("logger", handle).await?;
            Ok(())
        })
    });
    isolated
        .plugin(quiet, None)
        .expect("spawn logger plugin")
        .join()
        .await
        .expect("logger plugin active");
    for name in [
        "internal/fiber.created",
        "internal/fiber.loading",
        "internal/fiber.active",
        "internal/fiber.disposed",
        "internal/provider.changed",
        "internal/service",
        "internal/effect.cleaned",
        "internal/drain.started",
        "internal/drain.finished",
        "internal/dispatch",
    ] {
        capture(&isolated, log, name).await;
    }
    isolated
}

#[tokio::test]
async fn spawn_emits_created_and_the_state_family_with_one_envelope() {
    let log = EventLog::default();
    let ctx = test_context(&log).await;

    let plug = plugin("born", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    let uid = handle.uid().expect("live uid");

    wait_until(|| !log.of_fiber("internal/fiber.active", "born").is_empty()).await;
    let created = log
        .of_fiber("internal/fiber.created", "born")
        .pop()
        .expect("created event");
    assert_eq!(created["uid"], json!(uid));
    assert_eq!(
        created["trace"],
        json!(uid),
        "envelope carries the O3 trace"
    );
    assert!(created["realm"].is_null(), "default label is null");

    let active = log
        .of_fiber("internal/fiber.active", "born")
        .pop()
        .expect("active state event");
    assert_eq!(active["state"], json!("active"));
    assert_eq!(active["uid"], json!(uid));
    assert_eq!(active["trace"], json!(uid));
    // Exactly one event per landed state: the loading event fired once on
    // the pending->loading transition.
    assert_eq!(
        log.of_fiber("internal/fiber.loading", "born").len(),
        1,
        "one named event per real transition"
    );
    ctx.stop().await;
}

#[tokio::test]
async fn dispose_lands_the_disposed_state_event() {
    let log = EventLog::default();
    let ctx = test_context(&log).await;

    let plug = plugin("mortal", vec![], |_ctx, _config| Box::pin(async { Ok(()) }));
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    let uid = handle.uid().expect("live uid");
    handle.dispose().await;

    wait_until(|| !log.of_fiber("internal/fiber.disposed", "mortal").is_empty()).await;
    let disposed = log
        .of_fiber("internal/fiber.disposed", "mortal")
        .pop()
        .expect("disposed event");
    assert_eq!(disposed["state"], json!("disposed"));
    assert_eq!(disposed["trace"], json!(uid));
    ctx.stop().await;
}

#[tokio::test]
async fn provider_change_and_withdrawal_fire_changed_and_drain_pair() {
    let log = EventLog::default();
    let ctx = test_context(&log).await;

    let provider = plugin("provider", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc", 41i64).await?;
            Ok(())
        })
    });
    let provider_handle = ctx.plugin(provider, None).expect("spawn provider");
    provider_handle.join().await.expect("provider active");

    let consumer = plugin("consumer", vec!["svc".into()], |_ctx, _config| {
        Box::pin(async { Ok(()) })
    });
    let consumer_handle = ctx.plugin(consumer, None).expect("spawn consumer");
    consumer_handle.join().await.expect("consumer active");

    let changes = log.of("internal/provider.changed");
    let first = changes
        .iter()
        .find(|payload| payload["service"] == json!("svc"))
        .expect("provider.changed on provide");
    assert_eq!(first["provided"], json!(true));
    assert_eq!(
        first["fiber"],
        json!("provider"),
        "the envelope names the mutating fiber"
    );
    assert!(
        !log.of("internal/service").is_empty(),
        "internal/service still fires as service.notified (D27 mapping)"
    );

    // Withdrawal: the provide-disposer drains the touched consumer, so the
    // drain pair brackets the join. Observation order across distinct event
    // names is not promised (C5); presence and payload are.
    provider_handle.dispose().await;
    wait_until(|| {
        !log.of("internal/drain.finished").is_empty()
            && log
                .of("internal/provider.changed")
                .iter()
                .any(|payload| payload["provided"] == json!(false))
    })
    .await;
    assert_eq!(
        log.of("internal/drain.started")[0]["consumers"],
        json!(1),
        "the consumer is drained by name count"
    );
    let finished = log.of("internal/drain.finished").pop().expect("finished");
    assert_eq!(finished["consumers"], json!(1));
    assert!(finished["duration"].is_u64(), "finished carries duration");
    consumer_handle.join().await.expect("consumer re-converged");
    ctx.stop().await;
}

#[tokio::test]
async fn teardown_disposer_fires_effect_cleaned_ok() {
    let log = EventLog::default();
    let ctx = test_context(&log).await;

    let plug = plugin("clean", vec![], |ctx, _config| {
        Box::pin(async move {
            let disposer: Disposer = Box::new(|| Box::pin(async {}));
            ctx.effect("pool", async { Ok(Some(disposer)) }).await?;
            Ok(())
        })
    });
    let handle = ctx.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    let uid = handle.uid().expect("live uid");
    handle.dispose().await;

    wait_until(|| !log.of("internal/effect.cleaned").is_empty()).await;
    let cleaned = log.of("internal/effect.cleaned").pop().expect("cleaned");
    assert_eq!(cleaned["label"], json!("pool"));
    assert_eq!(cleaned["ok"], json!(true));
    assert_eq!(cleaned["trace"], json!(uid));
    assert!(cleaned["duration"].is_u64());
    ctx.stop().await;
}

#[tokio::test]
async fn failing_disposer_reports_ok_false() {
    let log = EventLog::default();
    let ctx = Context::new_with_options(dougong::ContextOptions {
        timeouts: dougong::TimeoutConfig {
            // A budget routes the disposer through a spawned task, so the
            // panic becomes a JoinError instead of killing the caller.
            cleanup: Some(Duration::from_secs(5)),
            ..dougong::TimeoutConfig::default()
        },
        ..dougong::ContextOptions::default()
    });
    let isolated = ctx.isolate("logger");
    let quiet = plugin("test-logger", vec![], move |ctx, _config| {
        let handle = LoggerHandle::new(Arc::new(Quiet));
        Box::pin(async move {
            ctx.provide("logger", handle).await?;
            Ok(())
        })
    });
    isolated
        .plugin(quiet, None)
        .expect("spawn logger")
        .join()
        .await
        .expect("logger active");
    capture(&isolated, &log, "internal/effect.cleaned").await;

    let plug = plugin("dirty", vec![], |ctx, _config| {
        Box::pin(async move {
            let disposer: Disposer = Box::new(|| {
                // Fault injection: contract violation must be contained as a JoinError.
                Box::pin(async {
                    panic!("disposer exploded"); // invariant check: fault-injection rail, not an error path
                })
            });
            ctx.effect("boom-pool", async { Ok(Some(disposer)) })
                .await?;
            Ok(())
        })
    });
    let handle = isolated.plugin(plug, None).expect("spawn");
    handle.join().await.expect("active");
    handle.dispose().await;

    wait_until(|| !log.of("internal/effect.cleaned").is_empty()).await;
    let cleaned = log.of("internal/effect.cleaned").pop().expect("cleaned");
    assert_eq!(cleaned["label"], json!("boom-pool"));
    assert_eq!(cleaned["ok"], json!(false), "a failed disposer is not ok");
    isolated.stop().await;
}

#[tokio::test]
async fn dispatch_event_carries_the_emitter_envelope() {
    let log = EventLog::default();
    let ctx = test_context(&log).await;

    let emitter = plugin("emitter", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.emit("evt.observed", Value::Null);
            Ok(())
        })
    });
    let handle = ctx.plugin(emitter, None).expect("spawn");
    handle.join().await.expect("active");
    let uid = handle.uid().expect("live uid");

    wait_until(|| {
        log.of("internal/dispatch")
            .iter()
            .any(|payload| payload["event"] == json!("evt.observed"))
    })
    .await;
    let dispatch = log
        .of("internal/dispatch")
        .into_iter()
        .find(|payload| payload["event"] == json!("evt.observed"))
        .expect("dispatch record");
    assert_eq!(dispatch["mode"], json!("emit"));
    assert_eq!(dispatch["trace"], json!(uid), "emitter trace rides along");
    ctx.stop().await;
}

/// Pins that the O4 additions left the O1 record face untouched: a
/// transition record still renders through `to_flat` without the event
/// family leaking into log records.
#[test]
fn log_records_are_not_events() {
    let record = LogRecord::new(LogLevel::Info, "fiber", "state_transition", "x".to_string());
    assert!(!record.to_flat().contains("internal/"));
}
