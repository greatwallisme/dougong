// Rust guideline compliant 2026-10-21
//! Multi-plugin composition demo (deepseek-harness parity).
//!
//! Real harnesses are not one plugin: they are many plugins with declared
//! inter-plugin dependencies, swappable capability seams, and event-driven
//! extension points (see `deepseek-harness/docs/architecture.md`). This
//! example walks the FULL developer workflow against two real document
//! files (`harness.json`, `harness.upper.json`, next to this file):
//!
//! 1. **Load** - boot NINE plugin slots from one document file: 7 in-process
//!    plugins (`settings`, `telemetry`, `tools`, `guard`, the `llm` seam,
//!    `agent-a`, `agent-b`) plus the greeter dylib and the greeter wasm
//!    component mixed into the same document;
//! 2. **Depend** - the agents declare `inject: [harness.llm, harness.tools,
//!    harness.settings]` and are wired reactively, regardless of row order;
//! 3. **Swap** - apply the second document (the `llm` row's config selects
//!    the `upper` adapter): the row hot-restarts and the consumers re-resolve
//!    the provider identity - "one provider swap changes the whole product";
//! 4. **Retire** - delete the `agent-b` row from the document: `apply`
//!    uninstalls exactly that slot (consumers drained first) and leaves the
//!    rest of the document untouched.
//!
//! Run: `cargo run -p dougong-loader --example harness_demo`
//! (needs `cargo build --workspace` and the wasip2 guest artifact).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dougong::{Context, Error, Injection, Result};
use dougong_loader::{Config, Reconciler, Source};
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

fn artifact_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend(["../../", "target"]);
    path
}

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// Loads one document file and resolves the artifact paths: the committed
/// documents reference build outputs relative to the workspace root, so the
/// example re-anchors them to this checkout's `target/` and runs from any
/// working directory.
fn load_document(file: &str) -> Result<Config> {
    let mut config = Config::load_file(&examples_dir().join(file))
        .map_err(|err| Error::msg(format!("load {file}: {err}")))?;
    let artifacts = artifact_dir();
    for spec in &mut config.plugins {
        let file_name = |path: &str| Path::new(path).file_name().unwrap_or_default().to_owned();
        match &mut spec.source {
            Source::Dylib { path } => {
                *path = artifacts
                    .join("debug")
                    .join(file_name(path))
                    .display()
                    .to_string();
            }
            Source::Wasm { path } => {
                *path = artifacts
                    .join("wasm32-wasip2/debug")
                    .join(file_name(path))
                    .display()
                    .to_string();
            }
            Source::Builtin { .. } => {}
        }
    }
    Ok(config)
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

    // Phase 1 - load: boot the whole harness from one document file
    // (in-process plugins, the dylib greeter, and the wasm greeter share the
    // same desired state).
    reconciler.replace(load_document("harness.json")?);
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

    // The agents declared their dependencies and were wired reactively -
    // row order in the document never mattered.
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

    // Phase 2 - swap: the new document selects the `upper` adapter for the
    // llm row. The reconciler hot-restarts that row, the consumers re-resolve
    // the provider identity, and the next turn is post-processed - no
    // consumer changes.
    reconciler.replace(load_document("harness.upper.json")?);
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

    // Phase 3 - retire: deleting a row from the document uninstalls exactly
    // that slot. Consumers of retired slots are drained first; everything
    // else in the document stays untouched.
    let mut doc = load_document("harness.json")?;
    doc.plugins.retain(|spec| spec.name != "agent-b");
    reconciler.replace(doc);
    let report = reconciler.apply(&ctx).await;
    assert_eq!(
        report.retired,
        vec!["agent-b".to_string()],
        "exactly the deleted row retired"
    );
    assert!(report.failures.is_empty());
    assert!(
        !reconciler.live().contains_key("agent-b"),
        "agent-b is gone"
    );
    handle(&reconciler, "agent-a")?.join().await?;
    log("agent-b retired by deleting its row; agent-a unaffected".to_string());

    ctx.stop().await;
    log("done: multi-plugin composition verified".to_string());
    Ok(())
}
