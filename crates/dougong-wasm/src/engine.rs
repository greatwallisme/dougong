// Rust guideline compliant 2026-10-21
//! Sandbox limits and the shared wasmtime engine/linker pair.

use std::time::Duration;

use dougong::{Error, Result};
use wasmtime::component::{HasSelf, Linker};
use wasmtime::{Config, Engine};

use crate::state::PluginState;

/// Hard resource caps for one wasm plugin (design decision D11).
///
/// Fuel bounds compute per export call; `StoreLimits` bounds memory and
/// instance resources; the epoch cadence bounds cooperative scheduling.
#[derive(Debug, Clone)]
pub struct WasmLimits {
    /// Compute budget charged to each guest export call.
    pub fuel_per_call: u64,
    /// Maximum linear-memory bytes one instance may map.
    pub memory_bytes: usize,
    /// Cadence of the epoch ticker backing cooperative yields.
    pub epoch_tick: Duration,
    /// Epoch increments a guest may run between yields.
    pub epoch_ticks_per_yield: u64,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            fuel_per_call: 50_000_000,
            memory_bytes: 64 * 1024 * 1024,
            epoch_tick: Duration::from_millis(5),
            epoch_ticks_per_yield: 1,
        }
    }
}

/// Shared wasmtime engine plus the dougong import linker.
///
/// One host serves any number of plugin loads; the engine and linker are
/// immutable after construction and the epoch ticker runs until dropped.
pub struct WasmHost {
    engine: Engine,
    linker: Linker<PluginState>,
    limits: WasmLimits,
    ticker: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for WasmHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmHost")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Drop for WasmHost {
    fn drop(&mut self) {
        // Abort the epoch ticker with its host: a detached interval task
        // would pin the engine alive forever (S2 leak sweep).
        self.ticker.abort();
    }
}

impl WasmHost {
    /// Builds the engine (component model, fuel, epoch) and registers the
    /// `dougong:kernel/host` imports.
    ///
    /// Must be called inside a tokio runtime: the kernel requires one and
    /// the epoch ticker is spawned onto it.
    ///
    /// # Errors
    ///
    /// Fails when the wasmtime engine cannot honor the configuration.
    pub fn new(limits: WasmLimits) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // `async_support` is always on in wasmtime 48 (the opt-in was
        // removed); fuel and epoch drive the sandbox budget.
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config)
            .map_err(|err| Error::transport("wasm_engine", format!("wasmtime engine: {err}")))?;

        let mut linker = Linker::new(&engine);
        crate::bindings::DougongPlugin::add_to_linker::<PluginState, HasSelf<PluginState>>(
            &mut linker,
            |state: &mut PluginState| state,
        )
        .map_err(|err| Error::transport("wasm_engine", format!("linker setup: {err}")))?;
        // Minimal WASI: the wasip2 guest runtime always declares wasi:io
        // imports; the definitions are inert because the per-instance
        // `WasiCtx` grants no capabilities.
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|err| Error::transport("wasm_engine", format!("wasi linker setup: {err}")))?;

        let ticker_engine = engine.clone();
        let tick = limits.epoch_tick;
        let ticker = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                ticker_engine.increment_epoch();
            }
        });

        Ok(Self {
            engine,
            linker,
            limits,
            ticker,
        })
    }

    /// The shared engine.
    #[must_use]
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The linker carrying the dougong host imports.
    #[must_use]
    pub const fn linker(&self) -> &Linker<PluginState> {
        &self.linker
    }

    /// The configured sandbox limits.
    #[must_use]
    pub const fn limits(&self) -> &WasmLimits {
        &self.limits
    }
}
