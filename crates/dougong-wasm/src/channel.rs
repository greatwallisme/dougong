// Rust guideline compliant 2026-10-21
//! Invocation channel from kernel-side callers into one guest component.
//!
//! All guest entries funnel through here so the sandbox budget is re-armed
//! on every export call and store exclusivity maps to a kernel error
//! instead of a wasmtime panic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use dougong::{BoxFuture, Error, Invoke, Result};
use parking_lot::Mutex;
use wasmtime::Store;

use crate::bindings::DougongPlugin;
use crate::bindings::dougong::kernel::types as wit;
use crate::error::{CONTRACT_ABI_HASH, to_kernel};
use crate::state::PluginState;

/// Store handle shared between the plugin object and the channel; the strong
/// count lives on the plugin, the channel holds a weak reference so a
/// retired instance can still be dropped.
pub type StoreArc = Arc<tokio::sync::Mutex<Store<PluginState>>>;

/// Re-arms fuel and the cooperative epoch deadline before one guest call.
pub fn arm_budget(store: &mut Store<PluginState>) -> Result<()> {
    let fuel = store.data().fuel_per_call();
    store
        .set_fuel(fuel)
        .map_err(|err| Error::msg(format!("set fuel: {err}")))?;
    store.epoch_deadline_async_yield_and_update(1);
    Ok(())
}

/// Kernel-side error for touching an instance that is already executing.
fn reentrant() -> Error {
    Error::new(
        "capability_denied",
        "reentrant call into a running wasm component",
    )
}

/// Trap (or abort) surfaced by a guest export call.
fn trapped(op: &str, err: &wasmtime::Error) -> Error {
    Error::msg(format!("wasm trap in {op}: {err}"))
}

/// Route from the kernel into one guest instance.
pub struct GuestChannel {
    store: OnceLock<Weak<tokio::sync::Mutex<Store<PluginState>>>>,
    exports: OnceLock<DougongPlugin>,
    /// Waterfall continuations handed to the guest, keyed by token.
    pending_next: Mutex<HashMap<u64, dougong::Next>>,
    /// Allocates effect handles (from `1`; `0` is the teardown reservation)
    /// and waterfall tokens from one counter.
    seed: AtomicU64,
    /// Tokio task id of the current export execution, if any. Written only
    /// by the store-lock holder, so it is the authoritative reentrancy
    /// marker for the executing instance.
    executing: Mutex<Option<tokio::task::Id>>,
}

impl std::fmt::Debug for GuestChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestChannel")
            .field("store", &self.store.get().is_some())
            .field("exports", &self.exports.get().is_some())
            .field("pending_next", &self.pending_next.lock().len())
            .field("seed", &self.seed.load(Ordering::Relaxed))
            .field("executing", &*self.executing.lock())
            .finish()
    }
}

/// Clears the execution marker when the export call ends, however it ends.
pub struct ExecutionGuard<'a> {
    pub executing: &'a Mutex<Option<tokio::task::Id>>,
}

impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        *self.executing.lock() = None;
    }
}

impl GuestChannel {
    // Not const: `HashMap::new` is not const-stable, and the token table
    // needs it.
    pub fn new() -> Self {
        Self {
            store: OnceLock::new(),
            exports: OnceLock::new(),
            pending_next: Mutex::new(HashMap::new()),
            seed: AtomicU64::new(0),
            executing: Mutex::new(None),
        }
    }

    /// Rejects a same-task reentry before it can block on the store lock:
    /// the calling task is the one currently executing the instance.
    fn reject_self_reentry(&self) -> Result<()> {
        let current = tokio::task::try_id();
        if self
            .executing
            .lock()
            .is_some_and(|owner| current.is_some_and(|task| owner == task))
        {
            return Err(reentrant());
        }
        Ok(())
    }

    /// Publishes the store weakly after the store is wrapped in its `Arc`.
    pub fn bind_store(&self, store: &StoreArc) {
        let _ = self.store.set(Arc::downgrade(store));
    }

    /// Publishes the instantiated component's export bindings.
    pub fn bind_exports(&self, exports: DougongPlugin) {
        let _ = self.exports.set(exports);
    }

    /// The component's export bindings.
    ///
    /// # Errors
    ///
    /// Fails when called before `bind_exports`, which cannot happen through
    /// the public load path.
    pub fn exports(&self) -> Result<&DougongPlugin> {
        self.exports
            .get()
            .ok_or_else(|| Error::msg("guest exports are not bound"))
    }

    fn store_arc(&self, service: &str) -> Result<StoreArc> {
        self.store
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| dougong::Error::inactive_dependency(service))
    }

    /// Borrow of the execution marker, for callers that hold the store lock
    /// directly (the activation path in the adapter).
    pub const fn execution_marker(&self) -> &Mutex<Option<tokio::task::Id>> {
        &self.executing
    }

    /// Invokes one dynamic-channel method on a provided service.
    pub async fn call_service(&self, name: &str, method: u32, payload: Vec<u8>) -> Result<Vec<u8>> {
        let store = self.store_arc(name)?;
        self.reject_self_reentry()?;
        let _executing = ExecutionGuard {
            executing: &self.executing,
        };
        let reply = {
            let mut guard = store.lock().await;
            // Write the marker only as the lock holder so waiters cannot
            // overwrite the id of the task that is actually executing.
            *self.executing.lock() = tokio::task::try_id();
            let key = wit::ServiceKey {
                name: name.to_string(),
                abi_hash: CONTRACT_ABI_HASH.to_vec(),
            };
            arm_budget(&mut guard)?;
            let reply = self
                .exports()?
                .dougong_kernel_plugin()
                .call_handle_service_call(&mut *guard, &key, method, &payload)
                .await
                .map_err(|err| trapped("handle-service-call", &err))?;
            // Release the store before surfacing guest errors.
            drop(guard);
            reply
        };
        reply.map_err(|err| to_kernel(&err))
    }

    /// Delivers one dispatched event to a registered guest listener.
    pub async fn handle_event(
        &self,
        name: &str,
        listener: u64,
        payload: Vec<u8>,
        token: u64,
    ) -> Result<Vec<u8>> {
        let store = self.store_arc(name)?;
        self.reject_self_reentry()?;
        let _executing = ExecutionGuard {
            executing: &self.executing,
        };
        let reply = {
            let mut guard = store.lock().await;
            *self.executing.lock() = tokio::task::try_id();
            let event = wit::EventKey {
                name: name.to_string(),
                abi_hash: CONTRACT_ABI_HASH.to_vec(),
            };
            arm_budget(&mut guard)?;
            let reply = self
                .exports()?
                .dougong_kernel_plugin()
                .call_handle_event(&mut *guard, &event, listener, &payload, Some(token))
                .await
                .map_err(|err| trapped("handle-event", &err))?;
            self.take_next(token);
            // Release the store before surfacing guest errors.
            drop(guard);
            reply
        };
        reply.map_err(|err| to_kernel(&err))
    }

    /// Runs one guest disposer (`0` = plugin teardown).
    pub async fn run_disposer(&self, handle: u64) -> Result<()> {
        let store = self.store_arc("(wasm instance)")?;
        self.reject_self_reentry()?;
        let _executing = ExecutionGuard {
            executing: &self.executing,
        };
        let reply = {
            let mut guard = store.lock().await;
            *self.executing.lock() = tokio::task::try_id();
            arm_budget(&mut guard)?;
            let reply = self
                .exports()?
                .dougong_kernel_plugin()
                .call_run_disposer(&mut *guard, handle)
                .await
                .map_err(|err| trapped("run-disposer", &err))?;
            // Release the store before surfacing guest errors.
            drop(guard);
            reply
        };
        reply.map_err(|err| to_kernel(&err))
    }

    /// Registers `next` under a fresh token and returns the token.
    pub fn alloc_next(&self, next: dougong::Next) -> u64 {
        let token = self.seed.fetch_add(1, Ordering::SeqCst) + 1;
        self.pending_next.lock().insert(token, next);
        token
    }

    /// Removes and returns the continuation stored for `token`.
    pub fn take_next(&self, token: u64) -> Option<dougong::Next> {
        self.pending_next.lock().remove(&token)
    }

    /// Allocates a fresh reversible-collection handle.
    pub fn alloc_handle(&self) -> u64 {
        self.seed.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// Kernel-side view of one provided wasm service: the dynamic channel
/// routed back into the component.
pub struct WasmInvoke {
    channel: Arc<GuestChannel>,
    service: String,
}

impl WasmInvoke {
    pub const fn new(channel: Arc<GuestChannel>, service: String) -> Self {
        Self { channel, service }
    }
}

impl Invoke for WasmInvoke {
    fn call(&self, method: u32, payload: Vec<u8>) -> BoxFuture<Result<Vec<u8>>> {
        let channel = Arc::clone(&self.channel);
        let service = self.service.clone();
        Box::pin(async move { channel.call_service(&service, method, payload).await })
    }
}
