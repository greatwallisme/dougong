// Rust guideline compliant 2026-10-21
//! Multi-plugin composition demo (deepseek-harness parity).
//!
//! Real harnesses are not one plugin: they are many plugins with declared
//! inter-plugin dependencies, swappable capability seams, and event-driven
//! extension points (see `deepseek-harness/docs/architecture.md`). This
//! example composes NINE plugin slots in ONE desired-state document and
//! proves the composition live:
//!
//! - in-process plugins: `settings` (config service), `telemetry`
//!   (capability event attached without importing anything), `llm` (the
//!   swappable seam), `tools` (registry), `guard` (policy attached purely
//!   via a waterfall veto - it injects nothing), `agent-a` / `agent-b`
//!   (consume `harness.llm` + `harness.tools` + settings persona);
//! - boundary sources mixed into the same document: the greeter dylib and
//!   the greeter wasm component (prerequisite: the wasip2 guest artifact);
//! - a **seam swap**: the second document replaces the `llm` row's builtin
//!   adapter (echo -> upper). `Reconciler::apply` hot-restarts the row, the
//!   agents re-resolve the provider identity reactively, and the next turn
//!   is post-processed - "one provider swap changes the whole product"
//!   without touching any consumer code.
//!
//! Run: `cargo run -p dougong-loader --example harness_demo`
//! (needs `cargo build --workspace` and the wasip2 guest artifact).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dougong::{Context, Error, Injection};
use dougong_loader::{Config, Reconciler};
use dougong_wasm::{WasmHost, WasmLimits};
use serde_json::{Value, json};

/// The swappable capability seam: every adapter implements this, consumers
/// never know which one is live (dsh: "one provider swap changes the whole
/// product").
trait Llm: Send + Sync {
    fn complete(&self, prompt: &str) -> String;
    fn adapter(&self) -> &'static str;
}

struct EchoLlm;
impl Llm for EchoLlm {
    fn complete(&self, prompt: &str) -> String {
        prompt.to_string()
    }
    fn adapter(&self) -> &'static str {
        "echo"
    }
}

struct UpperLlm;
impl Llm for UpperLlm {
    fn complete(&self, prompt: &str) -> String {
        prompt.to_ascii_uppercase()
    }
    fn adapter(&self) -> &'static str {
        "upper"
    }
}

/// Shared tool registry: tools register here through the typed service, the
/// agent executes through it after a policy waterfall.
#[derive(Default)]
struct ToolRegistry {
    names: Mutex<BTreeMap<String, String>>,
}

impl ToolRegistry {
    fn register(&self, name: &str, description: &str) {
        self.names
            .lock()
            .expect("registry lock")
            .insert(name.to_string(), description.to_string());
    }
    fn describe(&self) -> Vec<String> {
        self.names
            .lock()
            .expect("registry lock")
            .keys()
            .cloned()
            .collect()
    }
}

/// Monotonic request id for the llm seam (observability only).
static REQUESTS: AtomicU64 = AtomicU64::new(0);

/// The seam provider: its config selects the adapter (`{"adapter": "echo"}`
/// or `"upper"`), so a config-only row change swaps the whole capability
/// while consumers stay untouched (dsh: a patch replaces the row's config).
fn llm_plugin() -> Arc<dyn dougong::Plugin> {
    dougong::plugin("llm", vec![], |ctx, config| {
        Box::pin(async move {
            let adapter: Arc<dyn Llm> = match config.get("adapter").and_then(Value::as_str) {
                Some("upper") => Arc::new(UpperLlm),
                Some("echo") => Arc::new(EchoLlm),
                other => {
                    return Err(Error::msg(format!(
                        "llm adapter must be \"echo\" or \"upper\", got {other:?}"
                    )));
                }
            };
            // The seam provider must stay registered for the fiber lifetime;
            // withdrawal (or a config-driven restart) unregisters it and
            // consumers re-resolve reactively.
            ctx.provide("harness.llm", adapter).await?;
            Ok(())
        })
    })
}

fn settings_plugin() -> Arc<dyn dougong::Plugin> {
    dougong::plugin("settings", vec![], |ctx, config| {
        let persona = config
            .get("persona")
            .and_then(Value::as_str)
            .unwrap_or("assistant")
            .to_string();
        Box::pin(async move {
            ctx.provide("harness.settings", json!({ "persona": persona }))
                .await?;
            Ok(())
        })
    })
}

fn telemetry_plugin() -> Arc<dyn dougong::Plugin> {
    dougong::plugin("telemetry", vec![], |ctx, _config| {
        Box::pin(async move {
            // Capability event attached without importing anything: the
            // telemetry plugin only listens, it never sees the agent code.
            let logger = ctx.logger();
            ctx.on(
                "harness/turn",
                Arc::new(move |_ctx, payload, _next| {
                    let logger = Arc::clone(&logger);
                    Box::pin(async move {
                        logger.log(
                            dougong::LogLevel::Info,
                            "telemetry",
                            &format!("turn observed: {payload}"),
                        );
                        Ok(Value::Null)
                    })
                }),
            )
            .await?;
            Ok(())
        })
    })
}

fn tools_plugin() -> Arc<dyn dougong::Plugin> {
    dougong::plugin("tools", vec![], |ctx, _config| {
        Box::pin(async move {
            let registry = Arc::new(ToolRegistry::default());
            registry.register("echo", "returns its input");
            registry.register("rm -rf /", "destructive: exists only to be vetoed");
            ctx.provide("harness.tools", registry).await?;
            Ok(())
        })
    })
}

/// Policy attached purely through the event surface: the guard injects
/// nothing and imports nothing - it vetoes destructive tool calls in the
/// `tools/pre-execute` waterfall.
fn guard_plugin() -> Arc<dyn dougong::Plugin> {
    dougong::plugin("guard", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.on(
                "tools/pre-execute",
                Arc::new(|ctx, payload, _next| {
                    let logger = ctx.logger();
                    Box::pin(async move {
                        let vetoed =
                            payload.get("tool").and_then(Value::as_str) == Some("rm -rf /");
                        if vetoed {
                            // Returning without pulling `Next` vetoes the chain:
                            // the guard refuses the call and the agent never runs it.
                            logger.log(
                                dougong::LogLevel::Warn,
                                "guard",
                                "vetoed destructive tool call",
                            );
                            Ok(json!({ "vetoed": true, "reason": "guard: destructive tool" }))
                        } else {
                            Ok(Value::Null)
                        }
                    })
                }),
            )
            .await?;
            Ok(())
        })
    })
}

/// The agent loop, in miniature: consume the llm seam + tools registry +
/// settings persona, run one waterfall-ruled turn per `agent/turn` dispatch.
fn agent_plugin(name: &'static str, label: &'static str) -> Arc<dyn dougong::Plugin> {
    let inject = vec![
        Injection::new("harness.llm"),
        Injection::new("harness.tools"),
        Injection::new("harness.settings"),
    ];
    dougong::plugin(name, inject, move |ctx, _config| {
        let label = label;
        Box::pin(async move {
            let llm = ctx.require::<Arc<dyn Llm>>("harness.llm")?;
            let tools = ctx.require::<Arc<ToolRegistry>>("harness.tools")?;
            let settings = ctx.require::<Value>("harness.settings")?;
            let persona = settings
                .get("persona")
                .and_then(Value::as_str)
                .unwrap_or("assistant")
                .to_string();
            let logger = ctx.logger();
            logger.log(
                dougong::LogLevel::Info,
                label,
                &format!(
                    "agent online: llm adapter = {}, tools = {:?}, persona = {persona}",
                    llm.adapter(),
                    tools.describe(),
                ),
            );
            let turn_logger = Arc::clone(&logger);

            ctx.on(
                "agent/turn",
                Arc::new(move |ctx, payload, _next| {
                    let llm = Arc::clone(&llm);
                    let tools = Arc::clone(&tools);
                    let persona = persona.clone();
                    let label = label;
                    let logger = Arc::clone(&turn_logger);
                    Box::pin(async move {
                        let request = REQUESTS.fetch_add(1, Ordering::SeqCst) + 1;
                        let prompt = payload
                            .get("prompt")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();

                        // Tool call guarded by a policy waterfall: the guard can
                        // veto without the agent ever importing its code.
                        let wanted_tool = "rm -rf /";
                        let verdict = ctx
                            .waterfall(
                                "tools/pre-execute",
                                json!({ "tool": wanted_tool, "input": "" }),
                                |p| {
                                    Box::pin(
                                        async move { Ok(json!({ "vetoed": false, "passed": p })) },
                                    )
                                },
                            )
                            .await?;
                        logger.log(
                            dougong::LogLevel::Info,
                            label,
                            &format!("tools/pre-execute verdict = {verdict}"),
                        );
                        let tool_result =
                            if verdict.get("vetoed").and_then(Value::as_bool) == Some(true) {
                                json!({ "tool": wanted_tool, "ran": false, "vetoed": true,
                                "reason": verdict.get("reason") })
                            } else {
                                json!({ "tool": wanted_tool, "ran": true, "vetoed": false })
                            };

                        let reply = llm.complete(&prompt);
                        logger.log(
                            dougong::LogLevel::Info,
                            label,
                            &format!("request {request}: reply = {reply:?} (persona {persona})"),
                        );
                        Ok(json!({
                            "agent": label,
                            "adapter": llm.adapter(),
                            "reply": reply,
                            "tools": tools.describe(),
                            "tool_result": tool_result,
                        }))
                    })
                }),
            )
            .await?;
            Ok(())
        })
    })
}

fn artifact_dir() -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend(["../../", "target"]);
    path
}

fn document(llm_adapter: &str) -> Config {
    let artifact_dir = artifact_dir();
    let dir = artifact_dir.display();
    let wasm = artifact_dir.join("wasm32-wasip2/debug/greeter_wasm.wasm");
    let doc = format!(
        r#"{{"plugins":[
        {{"name":"settings","source":{{"kind":"builtin","name":"settings"}},"config":{{"persona":"dougong-miniharness"}},"isolate":null}},
        {{"name":"telemetry","source":{{"kind":"builtin","name":"telemetry"}},"config":null,"isolate":null}},
        {{"name":"tools","source":{{"kind":"builtin","name":"tools"}},"config":null,"isolate":null}},
        {{"name":"guard","source":{{"kind":"builtin","name":"guard"}},"config":null,"isolate":null}},
        {{"name":"llm","source":{{"kind":"builtin","name":"llm"}},"config":{{"adapter":"{llm_adapter}"}},"isolate":null}},
        {{"name":"agent-a","source":{{"kind":"builtin","name":"agent"}},"config":{{"label":"A"}},"isolate":null}},
        {{"name":"agent-b","source":{{"kind":"builtin","name":"agent"}},"config":{{"label":"B"}},"isolate":null}},
        {{"name":"greeter-dylib","source":{{"kind":"dylib","path":"{dir}/debug/libgreeter_plugin.so"}},"config":null,"isolate":null}},
        {{"name":"greeter-wasm","source":{{"kind":"wasm","path":"{}"}},"config":null,"isolate":null}}
    ]}}"#,
        wasm.display(),
    );
    Config::parse(&doc).expect("harness document is valid")
}

fn builtins() -> HashMap<String, Arc<dyn dougong::Plugin>> {
    [
        ("settings", settings_plugin()),
        ("telemetry", telemetry_plugin()),
        ("tools", tools_plugin()),
        ("guard", guard_plugin()),
        ("agent", agent_plugin("agent", "agent")),
        ("llm", llm_plugin()),
    ]
    .into_iter()
    .map(|(name, plugin)| (name.to_string(), plugin))
    .collect()
}

fn handle<'a>(reconciler: &'a Reconciler, name: &str) -> Result<&'a dougong::FiberHandle, Error> {
    reconciler
        .live()
        .get(name)
        .map(|live| &live.handle)
        .ok_or_else(|| Error::msg(format!("slot {name} is not live after reconcile")))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = WasmHost::new(WasmLimits::default())?;
    let ctx = Context::new();
    let logger = ctx.logger();
    let log = |message: String| {
        logger.log(dougong::LogLevel::Info, "harness-demo", &message);
    };
    let mut reconciler = Reconciler::new(builtins(), Some(Arc::new(host)));

    // Boot the whole harness from one document: in-process plugins, the
    // dylib greeter, and the wasm greeter share the same desired state.
    reconciler.replace(document("echo"));
    let report = reconciler.apply(&ctx).await;
    assert!(
        report.failures.is_empty(),
        "every slot must start: {:?}",
        report.failures
    );
    assert_eq!(report.inserted.len(), 9, "all nine slots inserted");
    for name in [
        "settings",
        "telemetry",
        "tools",
        "guard",
        "llm",
        "agent-a",
        "agent-b",
    ] {
        handle(&reconciler, name)?.join().await?;
    }
    log("booted: 9 slots from one document (builtin + dylib + wasm)".to_string());

    // Turn 1 over the echo adapter: the waterfall dispatch reaches both
    // agents (same event, two independent plugin instances).
    let first = ctx
        .waterfall("agent/turn", json!({ "prompt": "hello harness" }), |p| {
            Box::pin(async move { Ok(p) })
        })
        .await?;
    assert_eq!(first.get("adapter").and_then(Value::as_str), Some("echo"));
    assert_eq!(
        first
            .get("tool_result")
            .and_then(|t| t.get("vetoed"))
            .and_then(Value::as_bool),
        Some(true),
        "the guard vetoed the destructive tool without any import"
    );
    log(format!("turn 1 (echo seam): {first}"));

    // Seam swap by config alone: replace the llm row's builtin adapter. The
    // reconciler hot-restarts the row, consumers re-resolve the provider
    // identity, and the next turn is post-processed - no consumer changes.
    reconciler.replace(document("upper"));
    let report = reconciler.apply(&ctx).await;
    assert_eq!(
        report.updated,
        vec!["llm".to_string()],
        "only the seam swapped"
    );
    assert!(report.converged() || report.unchanged.len() == 8);
    handle(&reconciler, "llm")?.join().await?;
    handle(&reconciler, "agent-a")?.join().await?;
    handle(&reconciler, "agent-b")?.join().await?;

    let second = ctx
        .waterfall("agent/turn", json!({ "prompt": "hello harness" }), |p| {
            Box::pin(async move { Ok(p) })
        })
        .await?;
    assert_eq!(
        second.get("adapter").and_then(Value::as_str),
        Some("upper"),
        "the swapped adapter is live after a config-only change"
    );
    assert_eq!(
        second.get("reply").and_then(Value::as_str),
        Some("HELLO HARNESS"),
        "consumers rewired to the new seam provider"
    );
    log(format!("turn 2 (upper seam after swap): {second}"));

    ctx.stop().await;
    log("done: multi-plugin composition verified".to_string());
    Ok(())
}
