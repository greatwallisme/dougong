// Rust guideline compliant 2026-10-21
//! Per-instance store state and the `dougong:kernel/host` import trait.
//!
//! Import methods return the flat WIT result types: sandbox-visible
//! failures travel as `kernel-error`, never as traps.

use std::sync::Arc;

use wasmtime::component::{Resource, ResourceTable};
use wasmtime::{StoreLimits, StoreLimitsBuilder};

use crate::bindings::dougong::kernel::host as wit_host;
use crate::bindings::dougong::kernel::types as wit;
use crate::channel::{GuestChannel, WasmInvoke};
use crate::engine::WasmLimits;
use crate::error::{decode, encode, to_log_level, to_wit};

/// Identity-only payload of a registration table entry: the capability
/// token is the entry itself, diagnostics live kernel-side.
struct RegistrationEntry;

/// Store data of one loaded wasm plugin.
pub struct PluginState {
    /// Minimal WASI context: no stdio, env, args, preopens, or network. The
    /// definitions stay in the linker because the wasip2 guest runtime
    /// declares them, but every capability is empty.
    wasi: wasmtime_wasi::WasiCtx,
    plugin_name: String,
    /// The plugin fiber's context; bound at `apply`, inert before.
    ctx: dougong::Context,
    bound: bool,
    /// Service names declared in the descriptor; the capability check for
    /// `provide-service`.
    provide: Vec<String>,
    fuel_per_call: u64,
    ticks_per_yield: u64,
    limits: StoreLimits,
    table: ResourceTable,
    channel: Arc<GuestChannel>,
}

impl std::fmt::Debug for PluginState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginState")
            .field("plugin_name", &self.plugin_name)
            .field("bound", &self.bound)
            .field("provide", &self.provide)
            .finish_non_exhaustive()
    }
}

impl PluginState {
    pub fn new(channel: Arc<GuestChannel>, limits: &WasmLimits) -> Self {
        Self {
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
            plugin_name: String::new(),
            ctx: dougong::Context::new(),
            bound: false,
            provide: Vec::new(),
            fuel_per_call: limits.fuel_per_call,
            ticks_per_yield: limits.epoch_ticks_per_yield,
            // The wasip2 adapter instantiates several core modules per
            // component (guest plus import shims); the cap still bounds
            // runaway instantiation.
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_bytes)
                .memories(4)
                .tables(16)
                .instances(16)
                .build(),
            table: ResourceTable::new(),
            channel,
        }
    }

    /// The per-call fuel budget (read by the arming helper).
    pub const fn fuel_per_call(&self) -> u64 {
        self.fuel_per_call
    }

    /// The cooperative-yield cadence in epoch ticks (read by the arming
    /// helper).
    pub const fn epoch_ticks_per_yield(&self) -> u64 {
        self.ticks_per_yield
    }

    /// The `StoreLimits` view installed as the store's resource limiter.
    pub const fn limits_mut(&mut self) -> &mut StoreLimits {
        &mut self.limits
    }

    /// Wires the fiber context in at activation time.
    pub fn bind(&mut self, ctx: dougong::Context, plugin_name: String) {
        self.ctx = ctx;
        self.plugin_name = plugin_name;
        self.bound = true;
    }

    /// Records the declared provide names after the descriptor read.
    pub fn set_provide(&mut self, provide: Vec<String>) {
        self.provide = provide;
    }

    const fn inactive() -> wit::KernelError {
        wit::KernelError::InactiveContext
    }

    /// Records one accepted registration, or degrades the table error to a
    /// kernel error (flat import signatures cannot trap).
    fn record_registration(
        &mut self,
    ) -> std::result::Result<Resource<wit_host::Registration>, wit::KernelError> {
        let entry = self
            .table
            .push(RegistrationEntry)
            .map_err(|err| wit::KernelError::Internal(format!("registration table: {err}")))?;
        Ok(Resource::new_own(entry.rep()))
    }
}

impl wit_host::Host for PluginState {
    async fn log(&mut self, level: wit::LogLevel, msg: String) {
        self.ctx
            .logger()
            .log(to_log_level(level), &self.plugin_name, &msg);
    }

    async fn call_service(
        &mut self,
        key: wit::ServiceKey,
        method: u32,
        payload: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, wit::KernelError> {
        if !self.bound {
            return Err(Self::inactive());
        }
        match self.ctx.require::<dougong::Invoker>(&key.name) {
            Ok(invoker) => match invoker.call(method, payload).await {
                Ok(reply) => Ok(reply),
                Err(err) => Err(to_wit(&err, Some(&key))),
            },
            Err(err) => Err(to_wit(&err, Some(&key))),
        }
    }

    async fn provide_service(
        &mut self,
        key: wit::ServiceKey,
    ) -> std::result::Result<Resource<wit_host::Registration>, wit::KernelError> {
        if !self.bound {
            return Err(Self::inactive());
        }
        if !self.provide.iter().any(|name| name == &key.name) {
            return Err(wit::KernelError::CapabilityDenied(format!(
                "service \"{}\" is not declared in the plugin descriptor",
                key.name
            )));
        }
        let invoker = dougong::Invoker::new(Arc::new(WasmInvoke::new(
            Arc::clone(&self.channel),
            key.name.clone(),
        )));
        match self.ctx.provide(&key.name, invoker).await {
            Ok(_guard) => self.record_registration(),
            Err(err) => Err(to_wit(&err, Some(&key))),
        }
    }

    async fn register_listener(
        &mut self,
        ev: wit::EventKey,
        listener: u64,
    ) -> std::result::Result<Resource<wit_host::Registration>, wit::KernelError> {
        if !self.bound {
            return Err(Self::inactive());
        }
        let channel = Arc::clone(&self.channel);
        let event_name = ev.name.clone();
        let bridge_name = ev.name.clone();
        // Bridge between kernel dispatch and the guest export: allocates a
        // waterfall token per invocation so the guest can pull the rest of
        // the chain through `waterfall-next`.
        let bridge: dougong::Listener = Arc::new(
            move |_ctx: dougong::Context, payload: serde_json::Value, next: dougong::Next| {
                let channel = Arc::clone(&channel);
                let name = bridge_name.clone();
                Box::pin(async move {
                    let token = channel.alloc_next(next);
                    let bytes = encode(&payload)?;
                    let reply = channel.handle_event(&name, listener, bytes, token).await?;
                    decode(&reply)
                })
            },
        );
        match self.ctx.on(&event_name, bridge).await {
            Ok(_guard) => self.record_registration(),
            Err(err) => Err(to_wit(&err, None)),
        }
    }

    async fn dispatch(
        &mut self,
        ev: wit::EventKey,
        mode: wit::EventMode,
        payload: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, wit::KernelError> {
        if !self.bound {
            return Err(Self::inactive());
        }
        let value = decode(&payload).map_err(|err| to_wit(&err, None))?;
        let ctx = self.ctx.clone();
        let name = ev.name.clone();
        let outcome = match mode {
            wit::EventMode::Emit => {
                ctx.emit(&name, value);
                serde_json::Value::Null
            }
            wit::EventMode::Parallel => ctx
                .parallel(&name, value)
                .await
                .map_err(|err| to_wit(&err, None))?,
            wit::EventMode::Serial => ctx
                .serial(&name, value)
                .await
                .map_err(|err| to_wit(&err, None))?,
            wit::EventMode::Bail => ctx
                .bail(&name, value)
                .await
                .map_err(|err| to_wit(&err, None))?,
            wit::EventMode::Waterfall => ctx
                .waterfall(&name, value, identity_fallback)
                .await
                .map_err(|err| to_wit(&err, None))?,
        };
        encode(&outcome).map_err(|err| to_wit(&err, None))
    }

    async fn waterfall_next(
        &mut self,
        token: u64,
        payload: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, wit::KernelError> {
        let Some(next) = self.channel.take_next(token) else {
            return Err(wit::KernelError::Internal(
                "unknown waterfall token".to_string(),
            ));
        };
        let value = decode(&payload).map_err(|err| to_wit(&err, None))?;
        let value = next.call(value).await.map_err(|err| to_wit(&err, None))?;
        encode(&value).map_err(|err| to_wit(&err, None))
    }

    async fn effect_collect(
        &mut self,
        label: String,
    ) -> std::result::Result<u64, wit::KernelError> {
        if !self.bound {
            return Err(Self::inactive());
        }
        let handle = self.channel.alloc_handle();
        let channel = Arc::clone(&self.channel);
        let ctx = self.ctx.clone();
        let disposer: dougong::Disposer = Box::new(move || {
            let channel = Arc::clone(&channel);
            Box::pin(async move {
                // Disposal failures surface in kernel logs through the
                // effect unwind; nothing further to propagate here.
                let _ = channel.run_disposer(handle).await;
            })
        });
        match ctx.effect(label, async move { Ok(Some(disposer)) }).await {
            Ok(_guard) => Ok(handle),
            Err(err) => Err(to_wit(&err, None)),
        }
    }
}

// The shared `types` interface carries no functions or resources, so its
// generated host trait is a marker; the linker bound requires it anyway.
impl crate::bindings::dougong::kernel::types::Host for PluginState {}

impl wasmtime_wasi::WasiView for PluginState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl wit_host::HostRegistration for PluginState {
    fn drop(
        &mut self,
        rep: wasmtime::component::Resource<wit_host::Registration>,
    ) -> impl std::future::Future<Output = wasmtime::Result<()>> + Send {
        // The guest dropped its capability token; release the table entry.
        // Nothing awaits, so the future resolves on first poll.
        let entry: Resource<RegistrationEntry> = Resource::new_own(rep.rep());
        std::future::ready(
            self.table
                .delete(entry)
                .map(|_| ())
                .map_err(|err| wasmtime::Error::msg(format!("registration table: {err}"))),
        )
    }
}

/// Pass-through waterfall fallback for guest-initiated dispatches.
fn identity_fallback(
    payload: serde_json::Value,
) -> dougong::BoxFuture<dougong::Result<serde_json::Value>> {
    Box::pin(async move { Ok(payload) })
}
