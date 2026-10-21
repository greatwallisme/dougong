// Rust guideline compliant 2026-10-21
//! Multi-plugin composition demo (deepseek-harness parity).
//!
//! Real harnesses are not one plugin: they are many plugins with declared
//! inter-plugin dependencies, swappable capability seams, per-session
//! capability sets, and event-driven extension points (see
//! `deepseek-harness/docs/architecture.md`). This example walks the FULL
//! developer workflow against two real document files (`harness.json`,
//! `harness.upper.json`, next to this file):
//!
//! 1. **Pending** - apply a partial document (agents without their llm
//!    providers): the agents park in `Pending` and wait;
//! 2. **Load** - apply the full document: ten slots come up in one pass -
//!    8 in-process plugins (settings, telemetry, tools, guard, two llm
//!    seam providers, agent-a, agent-b) plus the greeter dylib and wasm
//!    component - and both agents converge automatically;
//! 3. **Realms** - `llm-a` and `agent-a` share `session-a`; `llm-b` and
//!    `agent-b` share `session-b`. The SAME service name `harness.llm`
//!    resolves per realm: realm A answers with the echo adapter, realm B
//!    with the upper adapter - per-session capability sets (D15);
//! 4. **Swap** - apply the second document (realm A's llm config selects
//!    `upper`): only that row hot-restarts and realm A's next turn is
//!    post-processed - "one provider swap changes the whole product";
//! 5. **Retire** - delete the `agent-b` row: `apply` uninstalls exactly
//!    that slot (consumers drained first) and the rest stays untouched.
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
            // Provides land in the row's realm (D15): the same seam name can
            // exist once per session without colliding.
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
            // Registered at the global default label: every realm falls back
            // to it for names its realm does not provide.
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
/// Turns are realm-tagged; an agent whose label does not match delegates
/// down the chain untouched.
fn agent_plugin() -> Arc<dyn dougong::Plugin> {
    let inject = vec![
        Injection::new("harness.llm"),
        Injection::new("harness.tools"),
        Injection::new("harness.settings"),
    ];
    dougong::plugin("agent", inject, |ctx, config| {
        // The row config names this instance: it doubles as the realm tag
        // the turn dispatcher routes by.
        let label = config
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_string();
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
                &label,
                &format!(
                    "agent online: llm adapter = {}, tools = {:?}, persona = {persona}",
                    llm.adapter(),
                    tools.describe(),
                ),
            );
            let turn_logger = Arc::clone(&logger);

            ctx.on(
                "agent/turn",
                Arc::new(move |ctx, payload, next| {
                    let llm = Arc::clone(&llm);
                    let tools = Arc::clone(&tools);
                    let persona = persona.clone();
                    let label = label.clone();
                    let logger = Arc::clone(&turn_logger);
                    Box::pin(async move {
                        // Realm routing: not our session -> delegate the turn
                        // to the rest of the chain untouched.
                        let realm = payload.get("realm").and_then(Value::as_str);
                        if realm != Some(&label) {
                            return next.call(payload).await;
                        }
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
                            &label,
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
        ("agent", agent_plugin()),
        ("llm", llm_plugin()),
    ]
    .into_iter()
    .map(|(name, plugin)| (name.to_string(), plugin))
    .collect()
}

fn handle(reconciler: &Reconciler, name: &str) -> Result<dougong::FiberHandle, Error> {
    reconciler
        .live()
        .get(name)
        .map(|live| live.handle.clone())
        .ok_or_else(|| Error::msg(format!("slot {name} is not live after reconcile")))
}

async fn dispatch_turn(ctx: &Context, realm: &str, prompt: &str) -> Result<Value, Error> {
    ctx.waterfall(
        "agent/turn",
        json!({ "realm": realm, "prompt": prompt }),
        |p| Box::pin(async move { Ok(p) }),
    )
    .await
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

    // Phase 1 - Pending: apply a PARTIAL document (the agents' llm providers
    // are absent). The agents park in `Pending` and wait - they do not fail.
    let mut partial = load_document("harness.json")?;
    partial
        .plugins
        .retain(|spec| !spec.name.starts_with("llm-"));
    reconciler.replace(partial);
    let report = reconciler.apply(&ctx).await;
    assert_eq!(report.inserted.len(), 8, "eight slots without the llm seam");
    let agent_a = handle(&reconciler, "agent-a")?;
    let agent_b = handle(&reconciler, "agent-b")?;
    for agent in [&agent_a, &agent_b] {
        // Expected to time out: the agents park until their providers show up.
        let _ = agent
            .join_with_timeout(std::time::Duration::from_millis(200))
            .await;
        assert_eq!(
            agent.state(),
            dougong::FiberState::Pending,
            "agents wait for their missing providers"
        );
    }
    log("phase 1: agents parked in Pending without their llm providers".to_string());

    // Phase 2 - load: the full document adds the llm seam providers; the
    // agents converge automatically, no restart, no re-ordering.
    reconciler.replace(load_document("harness.json")?);
    let report = reconciler.apply(&ctx).await;
    assert_eq!(
        report.inserted,
        vec!["llm-a".to_string(), "llm-b".to_string()],
        "exactly the two llm rows inserted"
    );
    for name in ["settings", "telemetry", "tools", "guard", "llm-a", "llm-b"] {
        handle(&reconciler, name)?.join().await?;
    }
    agent_a.join().await?;
    agent_b.join().await?;
    log("phase 2: both agents converged with their realm llm seams".to_string());

    // Phase 3 - realms: the same seam name resolves per session. Realm A
    // answers with the echo adapter, realm B with the upper adapter.
    let realm_a = dispatch_turn(&ctx, "A", "hello harness").await?;
    assert_eq!(
        realm_a.get("adapter").and_then(Value::as_str),
        Some("echo"),
        "realm A runs the echo adapter"
    );
    assert_eq!(
        realm_a
            .get("tool_result")
            .and_then(|t| t.get("vetoed"))
            .and_then(Value::as_bool),
        Some(true),
        "the guard vetoed the destructive tool without any import"
    );
    let realm_b = dispatch_turn(&ctx, "B", "hello harness").await?;
    assert_eq!(
        realm_b.get("adapter").and_then(Value::as_str),
        Some("upper"),
        "realm B runs the upper adapter"
    );
    log("phase 3: per-realm capability sets verified (A=echo, B=upper)".to_string());

    // Phase 4 - swap: realm A's llm config selects `upper`. Only that row
    // hot-restarts; realm A's next turn is post-processed - no consumer
    // changes, realm B untouched.
    reconciler.replace(load_document("harness.upper.json")?);
    let report = reconciler.apply(&ctx).await;
    assert_eq!(
        report.updated,
        vec!["llm-a".to_string()],
        "only realm A's seam swapped"
    );
    handle(&reconciler, "llm-a")?.join().await?;
    agent_a.join().await?;
    let swapped = dispatch_turn(&ctx, "A", "hello harness").await?;
    assert_eq!(
        swapped.get("adapter").and_then(Value::as_str),
        Some("upper"),
        "the swapped adapter is live after a config-only change"
    );
    assert_eq!(
        swapped.get("reply").and_then(Value::as_str),
        Some("HELLO HARNESS"),
        "realm A's consumers rewired to the new seam provider"
    );
    log("phase 4: realm A seam swapped by config alone".to_string());

    // Phase 5 - retire: delete the agent-b row; apply uninstalls exactly
    // that slot (consumers drained first) and the rest stays untouched.
    let mut doc = load_document("harness.upper.json")?;
    doc.plugins.retain(|spec| spec.name != "agent-b");
    reconciler.replace(doc);
    let report = reconciler.apply(&ctx).await;
    assert_eq!(
        report.retired,
        vec!["agent-b".to_string()],
        "exactly the deleted row retired"
    );
    assert!(!reconciler.live().contains_key("agent-b"));
    agent_a.join().await?;
    log("phase 5: agent-b retired by deleting its row; agent-a unaffected".to_string());

    ctx.stop().await;
    log("done: multi-plugin composition verified".to_string());
    Ok(())
}
