// Rust guideline compliant 2026-10-21
//! Kernel-facing adapter: one wasm component as a dougong [`Plugin`].

use std::borrow::Cow;
use std::sync::Arc;

use dougong::{BoxFuture, Context, Disposer, Error, Injection, Plugin, Result};
use serde_json::Value;

use crate::bindings::dougong::kernel::types as wit;
use crate::channel::{GuestChannel, StoreArc, arm_budget};

/// One loaded wasm plugin; started like any in-process plugin.
pub struct WasmPlugin {
    name: String,
    version: String,
    inject: Vec<Injection>,
    provide: Vec<String>,
    channel: Arc<GuestChannel>,
    store: StoreArc,
}

impl std::fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("provide", &self.provide)
            .finish_non_exhaustive()
    }
}

impl WasmPlugin {
    /// Assembles the adapter after a successful instantiation and
    /// descriptor read.
    pub const fn new(
        name: String,
        version: String,
        inject: Vec<Injection>,
        provide: Vec<String>,
        channel: Arc<GuestChannel>,
        store: StoreArc,
    ) -> Self {
        Self {
            name,
            version,
            inject,
            provide,
            channel,
            store,
        }
    }
}

impl Plugin for WasmPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }

    fn inject(&self) -> Vec<Injection> {
        self.inject.clone()
    }

    fn body_abort_safe(&self) -> bool {
        // The body holds the store mutex across awaited export calls and
        // wasmtime forbids abandoning an in-flight `call_async`, so the
        // driver must never drop the body mid-flight: a hung guest body is
        // awaited, bounded by the fuel budget (D11).
        false
    }

    fn apply(&self, ctx: Context, config: Value) -> BoxFuture<Result<()>> {
        let channel = Arc::clone(&self.channel);
        let store = Arc::clone(&self.store);
        let plugin_name = self.name.clone();
        Box::pin(async move {
            // Teardown is registered before any guest effect so the LIFO
            // unwind runs it last, after every provide/listener disposer
            // (mirrors the dylib adapter ordering).
            let teardown_channel = Arc::clone(&channel);
            let teardown: Disposer = Box::new(move || {
                let channel = Arc::clone(&teardown_channel);
                Box::pin(async move {
                    let _ = channel.run_disposer(0).await;
                })
            });
            ctx.effect("wasm.teardown", async move { Ok(Some(teardown)) })
                .await?;

            let site = wit::CallContext {
                fiber: ctx
                    .fiber_uid()
                    .map_or_else(String::new, |uid| uid.to_string()),
                epoch: ctx.fiber_epoch().unwrap_or_default(),
            };
            let config_bytes = crate::error::encode(&config)?;

            // One activation executes under one store lock; host imports
            // called during `apply` receive the store data directly and
            // never re-lock. The execution marker lets concurrent consumers
            // wait out the activation while same-task reentry is rejected.
            let executing = channel.execution_marker();
            let _executing = crate::channel::ExecutionGuard { executing };
            // The guard must span the export call: wasmtime stores admit one
            // execution at a time, and releasing early would let a second
            // activation enter mid-apply.
            {
                let mut guard = store.lock().await;
                // Written only as the lock holder (see `GuestChannel`).
                *executing.lock() = tokio::task::try_id();
                guard.data_mut().bind(ctx, plugin_name);
                arm_budget(&mut guard)?;
                let applied = channel
                    .exports()?
                    .dougong_kernel_plugin()
                    .call_apply(&mut *guard, &site, &config_bytes)
                    .await
                    .map_err(|err| Error::msg(format!("wasm trap in apply: {err}")))?;
                // Release the store before surfacing guest errors.
                drop(guard);
                applied.map_err(|err| crate::error::to_kernel(&err))?;
            }
            Ok(())
        })
    }
}
