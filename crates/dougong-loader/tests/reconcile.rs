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
