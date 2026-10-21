// Rust guideline compliant 2026-10-21
//! dylib/so plugin host for the dougong kernel.
//!
//! Loads a [`dougong_abi::PluginExports`] table from a shared library and
//! adapts it to a [`dougong::Plugin`]. The boundary follows the inverted-API
//! principle: the host owns all framework semantics (effects, provide,
//! reactive notification); the library only computes through the C-ABI
//! contract and links nothing but `dougong-abi`.
//!
//! # Service surface
//!
//! The plugin descriptor declares `provide` names; for each of them the host
//! registers a [`dougong::Invoker`] service so any consumer can call the
//! plugin through the dynamic channel (`method` selector + byte payload).
//!
//! # Event surface (ABI v2, design decision D14)
//!
//! The descriptor declares `listen` as an object mapping event names to u32
//! event ids; the host registers a mode-aware bridge per entry. In
//! waterfall, the plugin returns [`dougong_abi::STATUS_DEFERRED`] to pull
//! the rest of the chain: the host resolves the asynchronous continuation
//! and re-enters through the `on_next` export, whose post-processed output
//! becomes the chain value at this listener.
//!
//! # Instance serialization
//!
//! Consumer fibers run on their own tasks, so the kernel's per-fiber driver
//! alone does not order cross-fiber calls into one handle. The host holds a
//! per-instance lock across every foreign entry (service calls, event
//! dispatches including deferred continuations, and teardown), so
//! plugin-private state needs no internal locking.
//!
//! # Lifetime
//!
//! Loading is trusted-code territory (design decision D5, user ruling R1):
//! libraries stay mapped for the whole process; unloading runs only the
//! plugin's `teardown`. Never unmap a library whose handles or disposers may
//! still run.

use std::path::Path;
use std::sync::Arc;

use dougong::{BoxFuture, DispatchListener, DispatchMode, Error, Plugin, Result};
use dougong_abi::{
    ABI_VERSION, MODE_BAIL, MODE_EMIT, MODE_PARALLEL, MODE_SERIAL, MODE_WATERFALL, PluginExports,
};
use libloading::Library;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

/// Serializes every foreign call into one plugin instance (see the crate
/// documentation: cross-fiber callers are not ordered by the kernel).
type InstanceLock = Arc<AsyncMutex<()>>;

/// Resolved export table plus the mapped library that backs it.
///
/// # Safety contract
///
/// The table points at immutable code/data in the mapped library. Clones
/// share one `Arc<Library>`, so the mapping outlives every use; call
/// ordering across fibers is provided by the per-instance lock held by the
/// adapters that carry the table.
#[derive(Clone)]
pub struct Exports {
    table: *const PluginExports,
    _lib: Option<Arc<Library>>,
}

// SAFETY: the raw pointer targets a `static` export table inside the mapped
// library; the `Arc<Library>` keeps that mapping alive for as long as this
// value lives. The table itself is immutable after load.
unsafe impl Send for Exports {}
// SAFETY: same reasoning as `Send` — the table is immutable after load.
unsafe impl Sync for Exports {}

impl std::fmt::Debug for Exports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exports")
            .field("abi_version", &self.abi_version())
            .finish_non_exhaustive()
    }
}

impl Exports {
    /// Upper bound for one output payload; a plugin needing more is a
    /// contract failure, not a reason to grow without end.
    const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

    /// Runs one export with a growable out-buffer, retrying on
    /// `STATUS_CAPACITY` (the buffer protocol's documented signal to retry
    /// with more room) until the payload fits or the bound is hit.
    fn growable_call(
        op: &str,
        mut invoke: impl FnMut(*mut u8, usize, *mut usize) -> i32,
    ) -> Result<Vec<u8>> {
        let mut cap = 4096_usize;
        loop {
            let mut buf = vec![0_u8; cap];
            let mut len = 0_usize;
            let status = invoke(buf.as_mut_ptr(), buf.len(), &raw mut len);
            match status {
                dougong_abi::STATUS_OK => return Ok(buf[..len].to_vec()),
                dougong_abi::STATUS_CAPACITY => {
                    cap = cap.saturating_mul(2);
                    if cap > Self::MAX_OUTPUT_BYTES {
                        return Err(Error::msg(format!(
                            "dylib {op} output exceeds {} bytes",
                            Self::MAX_OUTPUT_BYTES
                        )));
                    }
                }
                other => {
                    return Err(Error::msg(format!("dylib {op} failed with status {other}")));
                }
            }
        }
    }

    const fn abi_version(&self) -> u32 {
        // SAFETY: the table pointer is valid for reads for the process
        // lifetime (see the struct contract).
        unsafe { (*self.table).abi_version }
    }

    fn name(&self) -> String {
        let table = self.table;
        // SAFETY: buffer contract of the export table; the callee writes at
        // most `cap` bytes and reports the used length.
        let outcome = Self::growable_call("name()", move |out, cap, len| unsafe {
            ((*table).name)(out, cap, len)
        });
        outcome.map_or_else(
            |_| "unnamed-dylib-plugin".to_string(),
            |bytes| String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn descriptor(&self) -> Result<Value> {
        let table = self.table;
        // SAFETY: see `growable_call` and the `Exports` struct contract.
        let bytes = Self::growable_call("descriptor()", move |out, cap, len| unsafe {
            ((*table).descriptor)(out, cap, len)
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|err| Error::msg(format!("dylib descriptor is not valid JSON: {err}")))
    }

    fn setup(&self) -> Option<Handle> {
        // SAFETY: setup takes no inputs and returns an owned opaque handle.
        let handle = unsafe { ((*self.table).setup)() };
        if handle.is_null() {
            None
        } else {
            Some(Handle(handle))
        }
    }

    fn call(&self, handle: &Handle, method: u32, payload: &[u8]) -> Result<Vec<u8>> {
        let table = self.table;
        let handle = handle.0;
        let op = format!("call(method={method})");
        // SAFETY: handle came from `setup` and was not yet torn down; buffer
        // contract as above.
        Self::growable_call(&op, move |out, cap, len| unsafe {
            ((*table).call)(
                handle,
                method,
                payload.as_ptr(),
                payload.len(),
                out,
                cap,
                len,
            )
        })
    }

    /// One event dispatch result: the plugin's status plus its output.
    fn dispatch_event(
        &self,
        handle: &Handle,
        event: u32,
        mode: u32,
        payload: &[u8],
        next_available: u32,
    ) -> Result<(i32, Vec<u8>)> {
        let table = self.table;
        let handle = handle.0;
        let mut cap = 4096_usize;
        loop {
            let mut buf = vec![0_u8; cap];
            let mut len = 0_usize;
            // SAFETY: handle came from `setup` and was not yet torn down;
            // buffer contract as above.
            let status = unsafe {
                ((*table).on_event)(
                    handle,
                    event,
                    mode,
                    payload.as_ptr(),
                    payload.len(),
                    next_available,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &raw mut len,
                )
            };
            match status {
                dougong_abi::STATUS_CAPACITY => {
                    cap = cap.saturating_mul(2);
                    if cap > Self::MAX_OUTPUT_BYTES {
                        return Err(Error::msg(format!(
                            "dylib on_event(event={event}) output exceeds {} bytes",
                            Self::MAX_OUTPUT_BYTES
                        )));
                    }
                }
                other => return Ok((other, buf[..len].to_vec())),
            }
        }
    }

    fn deliver_next(
        &self,
        handle: &Handle,
        event: u32,
        chain_status: i32,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let table = self.table;
        let handle = handle.0;
        let mut cap = 4096_usize;
        loop {
            let mut buf = vec![0_u8; cap];
            let mut len = 0_usize;
            // SAFETY: handle came from `setup`; the deferred continuation is
            // still open (see the instance-lock contract); buffer protocol
            // as above.
            let status = unsafe {
                ((*table).on_next)(
                    handle,
                    event,
                    chain_status,
                    data.as_ptr(),
                    data.len(),
                    buf.as_mut_ptr(),
                    buf.len(),
                    &raw mut len,
                )
            };
            match status {
                dougong_abi::STATUS_CAPACITY => {
                    cap = cap.saturating_mul(2);
                    if cap > Self::MAX_OUTPUT_BYTES {
                        return Err(Error::msg(format!(
                            "dylib on_next(event={event}) output exceeds {} bytes",
                            Self::MAX_OUTPUT_BYTES
                        )));
                    }
                }
                dougong_abi::STATUS_OK => return Ok(buf[..len].to_vec()),
                dougong_abi::STATUS_DEFERRED => {
                    return Err(Error::msg(
                        "dylib on_next returned STATUS_DEFERRED, which is a contract violation",
                    ));
                }
                other => {
                    return Err(Error::msg(format!(
                        "dylib on_next(event={event}) failed with status {other}"
                    )));
                }
            }
        }
    }

    fn teardown(&self, handle: &Handle) {
        // SAFETY: called exactly once per successful `setup`, per the
        // export-table contract.
        unsafe { ((*self.table).teardown)(handle.0) };
    }
}

/// Opaque plugin-private handle returned by the `setup` export.
#[derive(Clone)]
struct Handle(*mut std::ffi::c_void);

// SAFETY: the handle is plugin-private state; every access funnels through
// the per-instance lock held by the adapters.
unsafe impl Send for Handle {}
// SAFETY: sharing the handle across threads is safe because dereferences
// happen only under the same per-instance lock (bridge closures hold it).
unsafe impl Sync for Handle {}

/// Maps a kernel dispatch mode onto its ABI constant.
const fn abi_mode(mode: DispatchMode) -> u32 {
    match mode {
        DispatchMode::Emit => MODE_EMIT,
        DispatchMode::Parallel => MODE_PARALLEL,
        DispatchMode::Serial => MODE_SERIAL,
        DispatchMode::Bail => MODE_BAIL,
        DispatchMode::Waterfall => MODE_WATERFALL,
    }
}

/// Dynamic-channel adapter forwarding `Invoke` calls into the dylib exports.
struct DylibInvoke {
    exports: Exports,
    handle: Handle,
    lock: InstanceLock,
}

// SAFETY: the raw handle targets plugin-private state; every access is
// ordered by the per-instance lock this adapter holds across each foreign
// call, so no two threads can enter the instance concurrently.
unsafe impl Send for DylibInvoke {}
// SAFETY: same contract as `Send`.
unsafe impl Sync for DylibInvoke {}

impl dougong::Invoke for DylibInvoke {
    fn call(&self, method: u32, payload: Vec<u8>) -> BoxFuture<Result<Vec<u8>>> {
        let exports = self.exports.clone();
        let handle = self.handle.clone();
        let lock = Arc::clone(&self.lock);
        Box::pin(async move {
            let _instance = lock.lock().await;
            exports.call(&handle, method, &payload)
        })
    }
}

/// A plugin loaded from a shared library.
pub struct DynPlugin {
    exports: Exports,
    name: String,
    inject: Vec<dougong::Injection>,
    provide: Vec<String>,
    listen: Vec<(String, u32)>,
}

impl std::fmt::Debug for DynPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynPlugin")
            .field("name", &self.name)
            .field("provide", &self.provide)
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}

impl Plugin for DynPlugin {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Owned(self.name.clone())
    }

    fn inject(&self) -> Vec<dougong::Injection> {
        self.inject.clone()
    }

    fn apply(&self, ctx: dougong::Context, _config: Value) -> BoxFuture<Result<()>> {
        let exports = self.exports.clone();
        let provide = self.provide.clone();
        let listen = self.listen.clone();
        let plugin_name = self.name.clone();
        Box::pin(async move {
            let Some(handle) = exports.setup() else {
                return Err(Error::msg(format!(
                    "dylib plugin \"{plugin_name}\" setup returned null"
                )));
            };
            let instance: InstanceLock = Arc::new(AsyncMutex::new(()));

            // Teardown runs exactly once per successful setup, after every
            // other effect (provide disposers, bridges) has drained — LIFO
            // places it first in the queue because it is registered here,
            // before the effects below.
            let teardown_exports = exports.clone();
            let teardown_handle = Handle(handle.0);
            let teardown_lock = Arc::clone(&instance);
            let disposer: dougong::Disposer = Box::new(move || {
                Box::pin(async move {
                    let _instance = teardown_lock.lock().await;
                    teardown_exports.teardown(&teardown_handle);
                })
            });
            ctx.effect("dylib.teardown", async move { Ok(Some(disposer)) })
                .await?;

            // Mode-aware bridges: one per declared listened event. The lock
            // spans the whole invocation, including a deferred waterfall
            // continuation, so plugin state cannot observe interleaving.
            for (event_name, event_id) in &listen {
                let bridge_exports = exports.clone();
                let bridge_handle = Handle(handle.0);
                let bridge_lock = Arc::clone(&instance);
                let event_id = *event_id;
                let bridge: DispatchListener = Arc::new(move |_ctx, payload, next, mode| {
                    let exports = bridge_exports.clone();
                    let handle = bridge_handle.clone();
                    let lock = Arc::clone(&bridge_lock);
                    Box::pin(async move {
                        let _instance = lock.lock().await;
                        let bytes = serde_json::to_vec(&payload)
                            .map_err(|err| Error::msg(format!("encode payload: {err}")))?;
                        let waterfall = mode == DispatchMode::Waterfall;
                        let (status, reply) = exports.dispatch_event(
                            &handle,
                            event_id,
                            abi_mode(mode),
                            &bytes,
                            u32::from(waterfall),
                        )?;
                        match status {
                            dougong_abi::STATUS_DEFERRED => {
                                if !waterfall {
                                    return Err(Error::msg(format!(
                                        "dylib plugin deferred event {event_id} outside waterfall"
                                    )));
                                }
                                // DEFERRED output, when present, is the
                                // payload the chain continues with; empty
                                // output continues with the original.
                                let value: Value = if reply.is_empty() {
                                    serde_json::from_slice(&bytes).map_err(|err| {
                                        Error::msg(format!("decode payload: {err}"))
                                    })?
                                } else {
                                    serde_json::from_slice(&reply).map_err(|err| {
                                        Error::msg(format!("decode payload: {err}"))
                                    })?
                                };
                                let continued = next.call(value).await?;
                                let chain_bytes = serde_json::to_vec(&continued)
                                    .map_err(|err| Error::msg(format!("encode payload: {err}")))?;
                                let processed = exports.deliver_next(
                                    &handle,
                                    event_id,
                                    dougong_abi::STATUS_OK,
                                    &chain_bytes,
                                )?;
                                serde_json::from_slice(&processed)
                                    .map_err(|err| Error::msg(format!("decode payload: {err}")))
                            }
                            dougong_abi::STATUS_FAILED => Err(Error::msg(format!(
                                "dylib on_event(event={event_id}) failed"
                            ))),
                            _ => serde_json::from_slice(&reply)
                                .map_err(|err| Error::msg(format!("decode payload: {err}"))),
                        }
                    })
                });
                ctx.on_dispatch(event_name, bridge).await?;
            }

            for service in &provide {
                let invoker_exports = exports.clone();
                let invoker_handle = handle.0;
                let invoker = dougong::Invoker::new(Arc::new(DylibInvoke {
                    exports: invoker_exports,
                    handle: Handle(invoker_handle),
                    lock: Arc::clone(&instance),
                }));
                ctx.provide(service, invoker).await?;
            }
            Ok(())
        })
    }
}

/// Loads a dougong plugin from a shared library.
///
/// The library must export `DOUGONG_PLUGIN_EXPORTS` with a matching
/// [`ABI_VERSION`]. Loading is trusted-code territory: dylib plugins run with
/// full process privileges (the sandbox boundary is the wasm plugin form).
///
/// # Errors
///
/// Fails when the library cannot be mapped, the export symbol is missing,
/// or the ABI version mismatches.
pub fn load(path: &Path) -> Result<Arc<dyn Plugin>> {
    // SAFETY: `Library::new` maps a shared library the user asked for;
    // dougong keeps every plugin mapped for the process lifetime (D5), so
    // the returned handle is never used to unmap code that may still run.
    let lib = unsafe { Library::new(path) }
        .map_err(|err| Error::msg(format!("dlopen {}: {err}", path.display())))?;
    let lib = Arc::new(lib);
    // SAFETY: the symbol lives in the just-mapped library, which the
    // `Arc<Library>` kept alongside keeps alive.
    let table: libloading::Symbol<*const PluginExports> =
        unsafe { lib.get(b"DOUGONG_PLUGIN_EXPORTS") }
            .map_err(|err| Error::msg(format!("missing DOUGONG_PLUGIN_EXPORTS: {err}")))?;
    let exports = Exports {
        table: *table,
        _lib: Some(lib),
    };
    Ok(Arc::new(from_exports(exports)?))
}

/// Adapts an already-resolved export table (used by tests and by hosts that
/// resolve symbols through other means).
///
/// # Errors
///
/// Fails when the ABI version mismatches or the descriptor is malformed.
pub fn from_exports(exports: Exports) -> Result<DynPlugin> {
    if exports.abi_version() != ABI_VERSION {
        return Err(Error::msg(format!(
            "ABI version mismatch: plugin exports {}, host expects {ABI_VERSION}",
            exports.abi_version()
        )));
    }
    let name = exports.name();
    let descriptor: Value = exports.descriptor()?;
    let provide = descriptor
        .get("provide")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let inject = descriptor
        .get("inject")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|name| dougong::Injection::new(name.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let listen = match descriptor.get("listen") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(map)) => {
            let mut listen = Vec::with_capacity(map.len());
            for (event_name, id) in map {
                let Ok(id) = u32::try_from(id.as_u64().unwrap_or_else(|| u64::from(u32::MAX) + 1))
                else {
                    return Err(Error::msg(format!(
                        "descriptor listen[\"{event_name}\"] must be a u32 event id"
                    )));
                };
                listen.push((event_name.clone(), id));
            }
            listen
        }
        Some(_) => {
            return Err(Error::msg(
                "descriptor listen must be an object of event name to u32 id",
            ));
        }
    };
    Ok(DynPlugin {
        exports,
        name,
        inject,
        provide,
        listen,
    })
}

/// Test-support constructor wrapping a static export table.
#[cfg(test)]
pub(crate) fn from_static(table: &'static PluginExports) -> Result<DynPlugin> {
    from_exports(Exports { table, _lib: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dougong::Context;
    use dougong_abi::{STATUS_DEFERRED, STATUS_OK, write_out};
    use serde_json::json;

    const FAKE_NAME: &[u8] = b"fake-plugin";

    unsafe extern "C" fn fake_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        // SAFETY: host buffer contract.
        if unsafe { write_out(FAKE_NAME, out, cap, len) } {
            STATUS_OK
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    unsafe extern "C" fn fake_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        // SAFETY: host buffer contract.
        if unsafe {
            write_out(
                br#"{"name":"fake-plugin","provide":["greeting"],"inject":[],"listen":{"ping":1}}"#,
                out,
                cap,
                len,
            )
        } {
            STATUS_OK
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    unsafe extern "C" fn fake_setup() -> *mut std::os::raw::c_void {
        // One live byte per activation; `fake_teardown` releases it, so the
        // full apply/unload cycle runs against a non-null handle.
        Box::into_raw(Box::new(0_u8)).cast::<std::os::raw::c_void>()
    }

    unsafe extern "C" fn fake_call(
        _handle: *mut std::os::raw::c_void,
        _method: u32,
        _data: *const u8,
        _len: usize,
        out: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32 {
        // SAFETY: host buffer contract.
        if unsafe { write_out(b"echo", out, cap, out_len) } {
            STATUS_OK
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    unsafe extern "C" fn fake_on_event(
        _handle: *mut std::os::raw::c_void,
        _event: u32,
        _mode: u32,
        _data: *const u8,
        _len: usize,
        next_available: u32,
        out: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32 {
        // Waterfall pulls through the CPS protocol: defer, and the host
        // re-enters via `on_next`.
        if next_available == 1 {
            return STATUS_DEFERRED;
        }
        // SAFETY: host buffer contract.
        if unsafe { write_out(b"\"pong\"", out, cap, out_len) } {
            STATUS_OK
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    unsafe extern "C" fn fake_on_next(
        _handle: *mut std::os::raw::c_void,
        _event: u32,
        chain_status: i32,
        data: *const u8,
        len: usize,
        out: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32 {
        if chain_status != dougong_abi::STATUS_OK {
            return dougong_abi::STATUS_FAILED;
        }
        // SAFETY: host buffer contract; uppercase the chain reply as a
        // visible post-processing step.
        let slice = unsafe { std::slice::from_raw_parts(data, len) };
        let mut upper = slice.to_vec();
        upper.make_ascii_uppercase();
        // SAFETY: host buffer contract.
        if unsafe { write_out(&upper, out, cap, out_len) } {
            STATUS_OK
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    unsafe extern "C" fn fake_teardown(handle: *mut std::os::raw::c_void) {
        if handle.is_null() {
            return;
        }
        // SAFETY: the handle was produced by `fake_setup` and torn down
        // exactly once per activation.
        drop(unsafe { Box::from_raw(handle.cast::<u8>()) });
    }

    static WRONG_VERSION: PluginExports = PluginExports {
        abi_version: ABI_VERSION + 1,
        name: fake_name,
        descriptor: fake_descriptor,
        setup: fake_setup,
        call: fake_call,
        on_event: fake_on_event,
        on_next: fake_on_next,
        teardown: fake_teardown,
    };

    static GOOD_VERSION: PluginExports = PluginExports {
        abi_version: ABI_VERSION,
        name: fake_name,
        descriptor: fake_descriptor,
        setup: fake_setup,
        call: fake_call,
        on_event: fake_on_event,
        on_next: fake_on_next,
        teardown: fake_teardown,
    };

    #[test]
    fn abi_gate_rejects_version_mismatch() {
        let err = from_static(&WRONG_VERSION).expect_err("gate must reject");
        assert!(err.to_string().contains("ABI version mismatch"));
    }

    #[test]
    fn good_table_parses_descriptor_and_name() {
        let plugin = from_static(&GOOD_VERSION).expect("gate accepts matching version");
        assert_eq!(plugin.name, "fake-plugin");
        assert_eq!(plugin.provide, vec!["greeting".to_string()]);
        assert!(plugin.inject.is_empty());
        assert_eq!(plugin.listen, vec![("ping".to_string(), 1_u32)]);
    }

    unsafe extern "C" fn bad_listen_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        // SAFETY: host buffer contract.
        if unsafe {
            write_out(
                br#"{"name":"fake","provide":[],"inject":[],"listen":{"ping":"x"}}"#,
                out,
                cap,
                len,
            )
        } {
            STATUS_OK
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    static BAD_LISTEN_TABLE: PluginExports = PluginExports {
        abi_version: ABI_VERSION,
        name: fake_name,
        descriptor: bad_listen_descriptor,
        setup: fake_setup,
        call: fake_call,
        on_event: fake_on_event,
        on_next: fake_on_next,
        teardown: fake_teardown,
    };

    #[test]
    fn malformed_listen_is_a_load_error() {
        let err = from_static(&BAD_LISTEN_TABLE).expect_err("malformed listen must fail load");
        assert!(err.to_string().contains("must be a u32 event id"));
    }

    #[tokio::test]
    async fn call_forwards_through_the_exports() {
        let plugin = from_static(&GOOD_VERSION).expect("adapter builds");
        let outcome = plugin.exports.call(&Handle(std::ptr::null_mut()), 0, b"hi");
        assert_eq!(outcome.expect("call ok"), b"echo");
    }

    #[tokio::test]
    async fn bridges_forward_mode_and_deferred_continuation() {
        let plugin: Arc<dyn Plugin> = Arc::new(from_static(&GOOD_VERSION).expect("adapter builds"));
        let ctx = Context::new();
        let handle = ctx.plugin(plugin, None).expect("spawn");
        handle.join().await.expect("active");

        // Serial mode: the plain reply path.
        let pong = ctx.serial("ping", json!("ping")).await.expect("serial");
        assert_eq!(pong, json!("pong"));

        // Waterfall with a DEFERRED pull: the plugin uppercases the chain
        // reply through `on_next`.
        let upper = ctx
            .waterfall("ping", json!("make-loud"), |payload| {
                Box::pin(async move { Ok(payload) })
            })
            .await
            .expect("waterfall");
        assert_eq!(upper, json!("MAKE-LOUD"));
        handle.dispose().await;
    }

    /// Yields `STATUS_CAPACITY` exactly once, then succeeds: the host must
    /// retry with a larger buffer instead of failing the call.
    static CAPACITY_ARM: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

    unsafe extern "C" fn capacity_then_ok_call(
        _handle: *mut std::os::raw::c_void,
        _method: u32,
        _data: *const u8,
        _len: usize,
        out: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32 {
        if CAPACITY_ARM.swap(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return dougong_abi::STATUS_CAPACITY;
        }
        // SAFETY: host buffer contract.
        if unsafe { write_out(b"second", out, cap, out_len) } {
            STATUS_OK
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    static RETRY_TABLE: PluginExports = PluginExports {
        abi_version: ABI_VERSION,
        name: fake_name,
        descriptor: fake_descriptor,
        setup: fake_setup,
        call: capacity_then_ok_call,
        on_event: fake_on_event,
        on_next: fake_on_next,
        teardown: fake_teardown,
    };

    #[test]
    fn call_retries_after_status_capacity() {
        let plugin = from_static(&RETRY_TABLE).expect("adapter builds");
        let outcome = plugin.exports.call(&Handle(std::ptr::null_mut()), 0, b"");
        assert_eq!(outcome.expect("retry succeeds"), b"second");
    }
}
