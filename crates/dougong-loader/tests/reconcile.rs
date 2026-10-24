// Rust guideline compliant 2026-10-21
//! Reconciliation behavior: lifecycle, scope sharing, failure isolation,
//! and document validation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dougong::{Context, Injection, Plugin, plugin};
use dougong_loader::{Config, Reconciler};
use serde_json::{Value, json};

/// A builtin plugin that records every config its body runs with.
fn recorder(log: Arc<Mutex<Vec<Value>>>) -> Arc<dyn Plugin> {
    plugin("recorder", vec![], move |_ctx, config| {
        let log = Arc::clone(&log);
        Box::pin(async move {
            log.lock().expect("log lock").push(config);
            Ok(())
        })
    })
}

#[tokio::test]
async fn lifecycle_insert_converge_update_retire() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let builtins = HashMap::from([("recorder".to_string(), recorder(Arc::clone(&log)))]);
    let mut reconciler = Reconciler::new(builtins, None);
    let ctx = Context::new();
    // The root context carries one fiber of its own; assert deltas only.
    let baseline = ctx.live_fibers();

    let doc = r#"{"plugins":[{"name":"app","source":{"kind":"builtin","name":"recorder"},"config":{"v":1}}]}"#;
    reconciler.replace(Config::parse(doc).expect("valid doc"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.inserted, vec!["app".to_string()]);
    reconciler
        .live()
        .get("app")
        .expect("app live")
        .handle
        .join()
        .await
        .expect("body ok");
    assert_eq!(*log.lock().expect("log lock"), vec![json!({"v": 1})]);

    // A second pass over the same document changes nothing.
    let report = reconciler.apply(&ctx).await;
    assert!(report.converged(), "second pass must be a no-op");

    // Config change hot-updates (kernel update = restart with new config).
    let doc = r#"{"plugins":[{"name":"app","source":{"kind":"builtin","name":"recorder"},"config":{"v":2}}]}"#;
    reconciler.replace(Config::parse(doc).expect("valid doc"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.updated, vec!["app".to_string()]);
    reconciler
        .live()
        .get("app")
        .expect("app still live")
        .handle
        .join()
        .await
        .expect("body ok");
    assert_eq!(
        *log.lock().expect("log lock"),
        vec![json!({"v": 1}), json!({"v": 2})]
    );

    // Dropping the slot retires the plugin.
    reconciler.replace(Config::parse(r#"{"plugins":[]}"#).expect("valid doc"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.retired, vec!["app".to_string()]);
    assert_eq!(ctx.live_fibers(), baseline);
    assert!(reconciler.live().is_empty());
}

#[tokio::test]
async fn same_scope_joins_one_isolation_label() {
    let builtins: std::collections::HashMap<String, Arc<dyn Plugin>> = [
        (
            "provider".to_string(),
            plugin("provider", vec![], |ctx, _config| {
                Box::pin(async move {
                    ctx.provide("svc", "in-realm".to_string()).await?;
                    Ok(())
                })
            }),
        ),
        (
            "consumer".to_string(),
            plugin("consumer", vec![Injection::new("svc")], |ctx, _config| {
                Box::pin(async move {
                    let svc = ctx.require::<String>("svc")?;
                    assert_eq!(&*svc, "in-realm");
                    Ok(())
                })
            }),
        ),
    ]
    .into_iter()
    .collect();
    let mut reconciler = Reconciler::new(builtins, None);
    let ctx = Context::new();

    let doc = r#"{"plugins":[
        {"name":"p","source":{"kind":"builtin","name":"provider"},"isolate":"realm"},
        {"name":"c","source":{"kind":"builtin","name":"consumer"},"isolate":"realm"}
    ]}"#;
    reconciler.replace(Config::parse(doc).expect("valid doc"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.inserted.len(), 2, "both slots start: {report:?}");
    for name in ["p", "c"] {
        reconciler
            .live()
            .get(name)
            .expect("live")
            .handle
            .join()
            .await
            .expect("body ok");
    }
    // The consumer resolved `svc`: same doc scope -> same isolation label.
    assert_eq!(
        reconciler.live().get("p").expect("provider").handle.state(),
        dougong::FiberState::Active
    );
}

#[tokio::test]
async fn failing_slot_does_not_block_others() {
    let builtins: std::collections::HashMap<String, Arc<dyn Plugin>> =
        std::collections::HashMap::from([(
            "recorder".to_string(),
            recorder(Arc::new(Mutex::new(Vec::new()))),
        )]);
    let mut reconciler = Reconciler::new(builtins, None);
    let ctx = Context::new();
    let baseline = ctx.live_fibers();

    let doc = r#"{"plugins":[
        {"name":"broken","source":{"kind":"dylib","path":"/nonexistent/libbroken.so"}},
        {"name":"app","source":{"kind":"builtin","name":"recorder"}}
    ]}"#;
    reconciler.replace(Config::parse(doc).expect("valid doc"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.inserted, vec!["app".to_string()]);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].0, "broken");
    assert_eq!(ctx.live_fibers(), baseline + 1);
}

#[test]
fn parse_rejects_duplicate_and_empty_names() {
    let dup = r#"{"plugins":[
        {"name":"x","source":{"kind":"builtin","name":"a"}},
        {"name":"x","source":{"kind":"builtin","name":"b"}}
    ]}"#;
    let err = Config::parse(dup).expect_err("duplicate rejected");
    assert!(err.to_string().contains("duplicate"));

    let empty = r#"{"plugins":[{"name":"","source":{"kind":"builtin","name":"a"}}]}"#;
    let err = Config::parse(empty).expect_err("empty name rejected");
    assert!(err.to_string().contains("must not be empty"));
}

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: std::time::Duration = std::time::Duration::from_secs(5);

/// Polls the condition with a hard ceiling: a hang fails, never flakes.
async fn wait_until(condition: impl Fn() -> bool + Send + Sync) {
    tokio::time::timeout(RAIL, async {
        while !condition() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition reached");
}

/// Fault injection (S7): repeated hot reload through the declarative path
/// converges every single round, and the body observes each config exactly.
#[tokio::test]
async fn hot_reload_storm_converges_every_round() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let builtins = HashMap::from([("recorder".to_string(), recorder(Arc::clone(&log)))]);
    let mut reconciler = Reconciler::new(builtins, None);
    let ctx = Context::new();

    let doc = r#"{"plugins":[{"name":"app","source":{"kind":"builtin","name":"recorder"},"config":{"v":0}}]}"#;
    reconciler.replace(Config::parse(doc).expect("valid doc"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.inserted, vec!["app".to_string()]);
    reconciler
        .live()
        .get("app")
        .expect("app live")
        .handle
        .join()
        .await
        .expect("initial body settled");

    for round in 1..=10_i32 {
        let doc = format!(
            r#"{{"plugins":[{{"name":"app","source":{{"kind":"builtin","name":"recorder"}},"config":{{"v":{round}}}}}]}}"#
        );
        reconciler.replace(Config::parse(&doc).expect("valid doc"));
        let report = reconciler.apply(&ctx).await;
        assert!(
            report.failures.is_empty(),
            "round {round} failures: {:?}",
            report.failures
        );
        assert_eq!(
            report.updated,
            vec!["app".to_string()],
            "round {round} hot-updates"
        );
        let handle = &reconciler.live().get("app").expect("app live").handle;
        handle.join().await.expect("body ok");
        let last = log.lock().expect("log lock").last().cloned();
        assert_eq!(
            last,
            Some(json!({"v": round})),
            "round {round} config landed"
        );
    }

    // A pass over the unchanged document is a no-op after the storm.
    let report = reconciler.apply(&ctx).await;
    assert!(report.converged(), "settled after the storm: {report:?}");
    let expected: Vec<Value> = (0..=10_i32).map(|v| json!({"v": v})).collect();
    assert_eq!(*log.lock().expect("log lock"), expected);
    ctx.stop().await;
}

/// Fault injection (S7): flipping a provider slot's scope retires and
/// re-inserts it (scope is baked into the fiber at start); a consumer pinned
/// to one realm activates and parks in lockstep, round after round.
#[tokio::test]
async fn scope_flip_storm_retires_reinserts_and_reconsumers() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let consumer_observed = Arc::clone(&observed);
    let builtins: HashMap<String, Arc<dyn Plugin>> = [
        (
            "provider".to_string(),
            plugin("provider", vec![], |ctx, _config| {
                Box::pin(async move {
                    ctx.provide("svc", "realm-value".to_string()).await?;
                    Ok(())
                })
            }),
        ),
        (
            "consumer".to_string(),
            plugin(
                "consumer",
                vec![Injection::new("svc")],
                move |ctx, _config| {
                    let observed = Arc::clone(&consumer_observed);
                    Box::pin(async move {
                        let svc = ctx.require::<String>("svc")?;
                        observed.lock().expect("log lock").push(svc.to_string());
                        Ok(())
                    })
                },
            ),
        ),
    ]
    .into_iter()
    .collect();
    let mut reconciler = Reconciler::new(builtins, None);
    let ctx = Context::new();

    let doc_with_provider_in = |realm: &str| {
        format!(
            r#"{{"plugins":[
                {{"name":"p","source":{{"kind":"builtin","name":"provider"}},"isolate":"{realm}"}},
                {{"name":"c","source":{{"kind":"builtin","name":"consumer"}},"isolate":"realm-a"}}
            ]}}"#
        )
    };

    reconciler.replace(Config::parse(&doc_with_provider_in("realm-a")).expect("valid doc"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.inserted.len(), 2, "both slots start: {report:?}");
    let consumer_handle = reconciler
        .live()
        .get("c")
        .expect("consumer live")
        .handle
        .clone();
    wait_until(|| consumer_handle.state() == dougong::FiberState::Active).await;

    for round in 1..=3_i32 {
        // Flip the provider out of the consumer's realm: retired here means
        // "re-inserted elsewhere"; the consumer's dependency vanishes
        // (realm services never leak) and it parks.
        reconciler.replace(Config::parse(&doc_with_provider_in("realm-b")).expect("valid doc"));
        let report = reconciler.apply(&ctx).await;
        assert_eq!(
            report.inserted,
            vec!["p".to_string()],
            "round {round} flip out"
        );
        assert_eq!(
            report.unchanged,
            vec!["c".to_string()],
            "consumer slot untouched"
        );
        wait_until(|| consumer_handle.state() == dougong::FiberState::Pending).await;

        // Flip back: re-insert into realm-a rewires the consumer.
        reconciler.replace(Config::parse(&doc_with_provider_in("realm-a")).expect("valid doc"));
        let report = reconciler.apply(&ctx).await;
        assert_eq!(
            report.inserted,
            vec!["p".to_string()],
            "round {round} flip back"
        );
        wait_until(|| consumer_handle.state() == dougong::FiberState::Active).await;
        let count = observed.lock().expect("log lock").len();
        assert_eq!(
            count,
            usize::try_from(round).expect("small") + 1,
            "round {round} rewired"
        );
    }

    // Every activation resolved the realm-local value; the uid of the
    // provider fiber changed on every flip (retire + re-insert, never a
    // config update).
    assert!(
        observed
            .lock()
            .expect("log lock")
            .iter()
            .all(|value| value == "realm-value")
    );
    ctx.stop().await;
}
