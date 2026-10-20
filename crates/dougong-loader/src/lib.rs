// Rust guideline compliant 2026-10-21
//! Declarative configuration and reconciliation for a set of dougong
//! plugins (design decision D9).
//!
//! The kernel already owns the intent surface — `ctx.plugin`,
//! `FiberHandle::update`, `ctx.uninstall`, `ctx.isolate`. This crate is the
//! file-format layer above it: one JSON document describes the *desired*
//! state of a plugin set, and [`Reconciler::apply`] diffs it against the
//! live fibers and drives the kernel until the two agree (insert, update,
//! retire; scope changes retire and re-insert). Because kernel
//! reconciliation is confluent, the outcome depends only on the final
//! desired document, never on the path taken to reach it.
//!
//! Watching files for changes is deliberately out of scope: a watcher is an
//! event source, not reconciliation logic. Any trigger — inotify, a config
//! map sync, a REPL command — feeds the new document to
//! [`Reconciler::replace`] and the same converge loop runs.
//!
//! # Document format
//!
//! ```json
//! {
//!   "plugins": [
//!     {
//!       "name": "greeter",
//!       "source": { "kind": "dylib", "path": "./libgreeter_plugin.so" },
//!       "config": { "greeting": "hi" },
//!       "isolate": null
//!     }
//!   ]
//! }
//! ```
//!
//! `source.kind` is `dylib`, `wasm`, or `builtin`; `builtin` resolves
//! against the registry handed to [`Reconciler::new`] (built-in plugins of
//! the embedding harness are managed by the same document as external
//! artifacts). Plugin requirements (`inject`/`provide`) are *not* declared
//! here — they come from each artifact's own descriptor, which the loader
//! must not override.

pub mod reconcile;

pub use reconcile::{Live, Reconciler, Report, Source};

use dougong::{Error, Result};
use serde::Deserialize;
use serde_json::Value;

/// Desired state of one plugin slot.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginSpec {
    /// Unique, non-empty identifier; the reconciliation key.
    pub name: String,
    /// Where the plugin object comes from.
    pub source: Source,
    /// Kernel config handed to the plugin body; object or null.
    #[serde(default)]
    pub config: Value,
    /// Isolation scope name; `None` starts the plugin in the ambient scope.
    #[serde(default)]
    pub isolate: Option<String>,
}

/// The full desired-state document.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// One slot per plugin, keyed uniquely by name.
    pub plugins: Vec<PluginSpec>,
}

impl Config {
    /// Parses and validates a document: names must be unique and non-empty,
    /// and the top level must be an object.
    ///
    /// # Errors
    ///
    /// Fails on malformed JSON, non-object top level, duplicate or empty
    /// plugin names, or missing `plugins`.
    pub fn parse(json: &str) -> Result<Self> {
        let parsed: Value =
            serde_json::from_str(json).map_err(|err| Error::msg(format!("config: {err}")))?;
        if !parsed.is_object() {
            return Err(Error::new("invalid_config", "config must be an object"));
        }
        let config: Self = serde_json::from_value(parsed)
            .map_err(|err| Error::msg(format!("config shape: {err}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Reads and parses a document from disk.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or [`Config::parse`] rejects it.
    pub fn load_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| Error::msg(format!("read {}: {err}", path.display())))?;
        Self::parse(&text)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for spec in &self.plugins {
            if spec.name.is_empty() {
                return Err(Error::new(
                    "invalid_config",
                    "plugin name must not be empty",
                ));
            }
            if !seen.insert(spec.name.as_str()) {
                return Err(Error::new(
                    "invalid_config",
                    format!("duplicate plugin name \"{}\"", spec.name),
                ));
            }
            if !spec.config.is_null() && !spec.config.is_object() {
                return Err(Error::new(
                    "invalid_config",
                    format!("plugin \"{}\" config must be an object or null", spec.name),
                ));
            }
        }
        Ok(())
    }
}
