// Rust guideline compliant 2026-10-21
//! wasm component plugin host for the dougong kernel.
//!
//! Loads a component implementing the `dougong:kernel` world (see
//! `wit/dougong-plugin.wit`) and adapts it to a [`dougong::Plugin`]. The
//! boundary follows the inverted-API principle: the host owns all framework
//! semantics (effects, reactive resolution, LIFO teardown); the component
//! only computes through the WIT contract and imports nothing else, so the
//! world is the whole authority surface (Component Model guarantee).
//!
//! # Sandbox (design decision D11)
//!
//! Every guest execution is bounded three ways, configured through
//! [`WasmLimits`]:
//!
//! - **fuel**: hard compute budget per export call; exhaustion traps.
//! - **epoch**: cooperative yield cadence, driven by a background ticker so
//!   long loops cannot starve the tokio runtime.
//! - **`StoreLimits`**: memory / table / instance caps, installed before
//!   instantiation.
//!
//! The world imports zero WASI: no filesystem, network, clock, or random.
//!
//! # Service surface
//!
//! The descriptor declares `provide` names; for each the host registers a
//! [`dougong::Invoker`] so consumers call through the dynamic channel, which
//! routes into the component's `handle-service-call` export. Guest
//! `call-service` imports resolve against the plugin fiber's own declared
//! dependencies (kernel capability check, TS Algorithm 6).
//!
//! # Reentrancy
//!
//! A wasm instance is a single-threaded sandbox: invoking a service provided
//! by the same component *while it is executing* fails with a kernel error
//! instead of reentering (wasmtime forbids concurrent entry into one store).
//! Cross-plugin call cycles deadlock for the same reason the in-process
//! kernel deadlocks on them: each side awaits the other's driver.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//!
//! use dougong::Context;
//! use dougong_wasm::{load, WasmHost, WasmLimits};
//!
//! # async fn demo(path: &Path) -> dougong::Result<()> {
//! let host = WasmHost::new(WasmLimits::default())?;
//! let greeter = load(&host, path).await?;
//! let ctx = Context::new();
//! let fiber = ctx.plugin(greeter, None)?;
//! fiber.join().await?;
//! ctx.stop().await;
//! # Ok(())
//! # }
//! ```

#[allow(
    missing_docs,
    clippy::pedantic,
    clippy::nursery,
    reason = "bindgen-generated WIT bindings: machine-written code is exempt \
              from the project's documentation and style lints; correctness \
              lints (clippy::all) stay active"
)]
mod bindings {
    use wasmtime::component::bindgen;

    bindgen!({
        world: "dougong-plugin",
        path: "../../wit",
        imports: { default: async },
        exports: { default: async },
    });
}

mod adapter;
mod channel;
mod engine;
mod error;
mod state;

use std::path::Path;
use std::sync::Arc;

use bindings::DougongPlugin;
use dougong::{Error, Injection, Plugin, Result};

pub use adapter::WasmPlugin;
pub use engine::{WasmHost, WasmLimits};

/// Loads a dougong plugin from a wasm component file.
///
/// The component must export the `dougong:kernel/plugin` interface of the
/// world in `wit/dougong-plugin.wit`; it may import nothing beyond
/// `dougong:kernel/host`. Instantiation, descriptor read, and capability
/// setup all happen here; the returned plugin starts a fiber like any
/// in-process plugin.
///
/// # Errors
///
/// Fails when the file is not a valid component, instantiation or the
/// descriptor export traps, or the sandbox limits reject the instantiation.
pub async fn load(host: &WasmHost, path: &Path) -> Result<Arc<dyn Plugin>> {
    let component = wasmtime::component::Component::from_file(host.engine(), path)
        .map_err(|err| Error::msg(format!("component {}: {err}", path.display())))?;

    let channel = Arc::new(channel::GuestChannel::new());
    let mut store = wasmtime::Store::new(
        host.engine(),
        state::PluginState::new(Arc::clone(&channel), host.limits()),
    );
    // Sandbox caps must be installed before instantiation: memory grows
    // during initialization.
    store.limiter(|data: &mut state::PluginState| data.limits_mut());
    channel::arm_budget(&mut store)?;
    let exports = DougongPlugin::instantiate_async(&mut store, &component, host.linker())
        .await
        .map_err(|err| Error::msg(format!("instantiate {}: {err}", path.display())))?;
    channel.bind_exports(exports);

    let descriptor = channel
        .exports()?
        .dougong_kernel_plugin()
        .call_descriptor(&mut store)
        .await
        .map_err(|err| Error::msg(format!("descriptor trap: {err}")))?;

    let provide: Vec<String> = descriptor
        .provide
        .iter()
        .map(|key| key.name.clone())
        .collect();
    let inject: Vec<Injection> = descriptor
        .inject
        .iter()
        .map(|key| Injection::new(key.name.clone()))
        .collect();
    store.data_mut().set_provide(provide.clone());
    channel::arm_budget(&mut store)?;

    let store = Arc::new(tokio::sync::Mutex::new(store));
    channel.bind_store(&store);
    Ok(Arc::new(WasmPlugin::new(
        descriptor.name,
        descriptor.version,
        inject,
        provide,
        channel,
        store,
    )))
}
