// Rust guideline compliant 2026-10-21
//! Diff-and-converge loop between a desired [`Config`](crate::Config) and
//! the live plugin fibers.

use std::collections::HashMap;
use std::sync::Arc;

use dougong::{Context, Error, FiberHandle, Plugin, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::PluginSpec;

/// Where a plugin object comes from.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Source {
    /// Shared library loaded through `dougong-dylib` (trusted code, D5).
    #[serde(rename = "dylib")]
    Dylib {
        /// Path of the `.so`/`.dylib`/`.dll` artifact.
        path: String,
    },
    /// Wasm component loaded through `dougong-wasm` (sandboxed, D11).
    #[serde(rename = "wasm")]
    Wasm {
        /// Path of the `.wasm` component binary.
        path: String,
    },
    /// Pre-registered in-process plugin resolved from the built-in
    /// registry; the harness's own plugins share the declarative path.
    #[serde(rename = "builtin")]
    Builtin {
        /// Key in the built-in registry handed to `Reconciler::new`.
        name: String,
    },
}

/// One live, reconciled plugin.
pub struct Live {
    /// The plugin object backing the fiber (needed for uninstall).
    pub plugin: Arc<dyn Plugin>,
    /// Handle of the started fiber (needed for config updates).
    pub handle: FiberHandle,
    /// Canonical serialization of the config the fiber runs with.
    pub fingerprint: String,
    /// Isolation scope the fiber was started in.
    pub isolate: Option<String>,
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Live")
            .field("plugin", &self.plugin.name())
            .field("handle", &self.handle)
            .field("fingerprint", &self.fingerprint)
            .field("isolate", &self.isolate)
            .finish()
    }
}

/// What one reconciliation pass did.
#[derive(Debug, Default)]
pub struct Report {
    /// Plugins started by this pass.
    pub inserted: Vec<String>,
    /// Plugins whose config changed and were hot-updated.
    pub updated: Vec<String>,
    /// Plugins uninstalled because the document dropped them.
    pub retired: Vec<String>,
    /// Plugins whose desired state matched the live state.
    pub unchanged: Vec<String>,
    /// Per-plugin failures; the pass continues with the remaining slots so
    /// a broken artifact cannot block the rest of the document.
    pub failures: Vec<(String, Error)>,
}

impl Report {
    /// True when the pass made no changes and recorded no failures.
    #[must_use]
    pub const fn converged(&self) -> bool {
        self.inserted.is_empty()
            && self.updated.is_empty()
            && self.retired.is_empty()
            && self.failures.is_empty()
    }
}

/// Drives the live plugin set toward a desired document.
pub struct Reconciler {
    builtins: HashMap<String, Arc<dyn Plugin>>,
    wasm: Option<Arc<dougong_wasm::WasmHost>>,
    /// Desired document installed by `replace`, converged by `apply`.
    pending: crate::Config,
    /// Isolation label per scope name, so every plugin declared in the same
    /// realm joins one label (`ctx.isolate` alone would mint a fresh label
    /// per call).
    scopes: HashMap<String, u64>,
    live: HashMap<String, Live>,
}

impl std::fmt::Debug for Reconciler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconciler")
            .field("builtins", &self.builtins.keys().collect::<Vec<_>>())
            .field("wasm_host", &self.wasm.is_some())
            .field(
                "pending",
                &self
                    .pending
                    .plugins
                    .iter()
                    .map(|spec| spec.name.clone())
                    .collect::<Vec<_>>(),
            )
            .field("scopes", &self.scopes.keys().collect::<Vec<_>>())
            .field("live", &self.live.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Reconciler {
    /// Creates a reconciler that resolves `builtin` sources from `builtins`
    /// and `wasm` sources on the shared host when one is provided.
    #[must_use]
    pub fn new(
        builtins: HashMap<String, Arc<dyn Plugin>>,
        wasm: Option<Arc<dougong_wasm::WasmHost>>,
    ) -> Self {
        Self {
            builtins,
            wasm,
            pending: crate::Config {
                plugins: Vec::new(),
            },
            scopes: HashMap::new(),
            live: HashMap::new(),
        }
    }

    /// Reconciles the current desired document against the live set.
    ///
    /// # Errors
    ///
    /// Never fails as a whole: per-slot failures land in
    /// [`Report::failures`] so one broken artifact cannot block the rest of
    /// the document.
    pub async fn apply(&mut self, ctx: &Context) -> Report {
        let mut report = Report::default();
        // Convergence is idempotent: the document stays installed until
        // `replace` swaps it, so every `apply` re-derives the diff.
        let desired = self.pending.clone();
        let mut wanted = std::collections::HashSet::new();

        for spec in &desired.plugins {
            wanted.insert(spec.name.clone());
            if self.live.contains_key(&spec.name) {
                self.reconcile_existing(ctx, spec, &mut report).await;
            } else {
                self.insert(ctx, spec, &mut report).await;
            }
        }

        let retired: Vec<String> = self
            .live
            .keys()
            .filter(|name| !wanted.contains(*name))
            .cloned()
            .collect();
        for name in retired {
            let Some(entry) = self.live.remove(&name) else {
                continue;
            };
            ctx.uninstall(&entry.plugin).await;
            report.retired.push(name);
        }
        report
    }

    /// Installs a new desired document; the next [`apply`](Self::apply)
    /// converges toward it.
    pub fn replace(&mut self, desired: crate::Config) {
        self.pending = desired;
    }

    /// The currently live entries, keyed by plugin name.
    #[must_use]
    pub const fn live(&self) -> &HashMap<String, Live> {
        &self.live
    }

    async fn reconcile_existing(&mut self, ctx: &Context, spec: &PluginSpec, report: &mut Report) {
        let Some(entry) = self.live.get(&spec.name) else {
            return;
        };
        let fingerprint = fingerprint(&spec.config);
        if entry.fingerprint == fingerprint && entry.isolate == spec.isolate {
            report.unchanged.push(spec.name.clone());
            return;
        }
        // Scope is baked into the fiber's context at start time; a scope
        // change therefore retires and re-inserts, which the confluence
        // theorem reduces to "the final state wins".
        if entry.isolate != spec.isolate {
            let Some(existing) = self.live.remove(&spec.name) else {
                return;
            };
            ctx.uninstall(&existing.plugin).await;
            self.insert(ctx, spec, report).await;
            return;
        }
        let Some(entry) = self.live.get(&spec.name) else {
            return;
        };
        match entry.handle.update(spec.config.clone()).await {
            Ok(_value) => {
                if let Some(entry) = self.live.get_mut(&spec.name) {
                    entry.fingerprint = fingerprint;
                }
                report.updated.push(spec.name.clone());
            }
            Err(err) => report
                .failures
                .push((spec.name.clone(), Error::msg(format!("update: {err}")))),
        }
    }

    async fn insert(&mut self, ctx: &Context, spec: &PluginSpec, report: &mut Report) {
        let plugin = match self.materialize(spec).await {
            Ok(plugin) => plugin,
            Err(err) => {
                report
                    .failures
                    .push((spec.name.clone(), Error::msg(format!("load: {err}"))));
                return;
            }
        };
        let slot = match &spec.isolate {
            Some(scope) => {
                let label = *self
                    .scopes
                    .entry(scope.clone())
                    .or_insert_with(|| ctx.fresh_label());
                ctx.isolate_realm(label)
            }
            None => ctx.clone(),
        };
        match slot.plugin(Arc::clone(&plugin), Some(spec.config.clone())) {
            Ok(handle) => {
                self.live.insert(
                    spec.name.clone(),
                    Live {
                        plugin,
                        handle,
                        fingerprint: fingerprint(&spec.config),
                        isolate: spec.isolate.clone(),
                    },
                );
                report.inserted.push(spec.name.clone());
            }
            Err(err) => report
                .failures
                .push((spec.name.clone(), Error::msg(format!("start: {err}")))),
        }
    }

    async fn materialize(&self, spec: &PluginSpec) -> Result<Arc<dyn Plugin>> {
        match &spec.source {
            Source::Dylib { path } => dougong_dylib::load(std::path::Path::new(path)),
            Source::Wasm { path } => {
                let Some(host) = &self.wasm else {
                    return Err(Error::new(
                        "no_wasm_host",
                        format!(
                            "plugin \"{}\" needs a wasm host; pass one to Reconciler::new",
                            spec.name
                        ),
                    ));
                };
                dougong_wasm::load(host, std::path::Path::new(path)).await
            }
            Source::Builtin { name } => self.builtins.get(name).cloned().ok_or_else(|| {
                Error::new(
                    "unknown_builtin",
                    format!("no builtin plugin named \"{name}\""),
                )
            }),
        }
    }
}

/// Canonical fingerprint of a config value: `serde_json` maps are ordered,
/// so `to_string` is stable across equal documents.
#[must_use]
pub fn fingerprint(config: &Value) -> String {
    config.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_order_insensitive() {
        let a: Value =
            serde_json::from_str(r#"{"a":1,"b":{"x":true,"y":null}}"#).expect("valid json");
        let b: Value =
            serde_json::from_str(r#"{"b":{"y":null,"x":true},"a":1}"#).expect("valid json");
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }
}
