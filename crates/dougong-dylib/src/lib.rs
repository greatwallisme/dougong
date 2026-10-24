// Rust guideline compliant 2026-10-23
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
//! # Outbound surface (ABI v3)
//!
//! Each activation receives a [`dougong_abi::HostExports`] vtable at `setup`:
//! `require` performs a synchronous blocking call into another plugin's
//! service (capability-checked against the descriptor `inject` set, resolved
//! in the activation's realm), and `emit` publishes a JSON event to the
//! kernel bus. Reentrancy is governed by a per-thread activation stack
//! (design decision D-V3-3 option b'): any foreign entry — `setup`, `call`,
//! `on_event`/`on_next`, `teardown` — records its instance for the duration
//! of the synchronous FFI segment, and a service call that would re-enter an
//! instance already executing on the calling thread fails deterministically
//! with `reentrancy_cycle` instead of deadlocking the per-instance lock.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Poll, Wake, Waker};

use dougong::{BoxFuture, DispatchListener, DispatchMode, Error, Invoker, Plugin, Result};
use dougong_abi::{
    ABI_VERSION, HostExports, MODE_BAIL, MODE_EMIT, MODE_PARALLEL, MODE_SERIAL, MODE_WATERFALL,
    PluginExports, STATUS_CAPACITY, STATUS_FAILED, STATUS_HOST_ERROR, STATUS_INACTIVE,
    STATUS_INVALID_EVENT, STATUS_INVALID_PAYLOAD, STATUS_OK, STATUS_REENTRANCY, STATUS_UNDECLARED,
    write_out,
};
use libloading::Library;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

/// Serializes every foreign call into one plugin instance (see the crate
/// documentation: cross-fiber callers are not ordered by the kernel).
type InstanceLock = Arc<AsyncMutex<()>>;

/// Per-process activation identity source for the reentrancy stack. A static
/// counter is the minimal correct tool: ids must be unique across
/// independently loaded libraries and carry no state beyond uniqueness, so
/// `Relaxed` ordering is sufficient.
static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

fn next_instance_id() -> u64 {
    NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed)
}

std::thread_local! {
    /// Instances whose foreign entries are executing on this thread right
    /// now (reentrancy policy D-V3-3 option b'). A deadlock needs one thread
    /// to re-acquire an instance lock it already holds, which is exactly
    /// "this instance is on this thread's stack". Guards never span `.await`
    /// points — each covers one synchronous FFI segment — so push/pop pairing
    /// always stays on one thread even when the surrounding task migrates.
    static ENTRY_STACK: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// True when `id` is executing a foreign entry on this thread right now.
fn entered_on_this_thread(id: u64) -> bool {
    ENTRY_STACK.with(|stack| stack.borrow().contains(&id))
}

/// RAII guard: pushes the instance id on creation, pops it on drop. Open it
/// after the last `.await` and drop it before the next one.
struct EntryGuard(u64);

impl EntryGuard {
    fn enter(id: u64) -> Self {
        ENTRY_STACK.with(|stack| stack.borrow_mut().push(id));
        Self(id)
    }
}

impl Drop for EntryGuard {
    fn drop(&mut self) {
        ENTRY_STACK.with(|stack| {
            let popped = stack.borrow_mut().pop();
            debug_assert_eq!(popped, Some(self.0), "entry stack pairing broken");
        });
    }
}

/// Host-side state of one plugin activation backing the v3 [`HostExports`]
/// vtable: built per activation, dead once that activation's `teardown`
/// returns (the vtable's documented lifetime bound).
struct Activation {
    /// The owning fiber's context: capability resolution and event dispatch
    /// run through it, so realm binding matches the in-process form (D15).
    ctx: dougong::Context,
    /// Runtime captured during `apply`; `enter`ing it gives foreign threads
    /// (plugin-owned, never inside tokio) a valid runtime context.
    rt: tokio::runtime::Handle,
    /// Descriptor `inject` names: the constant capability-check basis for
    /// outbound `require` (the in-process form uses the fiber interest set;
    /// the boundary form uses the descriptor set — see the equivalence table
    /// in `doc/research/dougong-design.md`).
    inject: Vec<String>,
}

/// `Send`/`Sync` wrapper for the raw `Activation` reference carried by
/// [`HostExports::host_handle`].
struct ActivationPtr(*mut std::ffi::c_void);

// SAFETY: the pointee is a shared, immutable `Activation` kept alive by the
// teardown disposer until that activation's `teardown` has returned; vtable
// calls after that moment are outside the contract (documented UB).
unsafe impl Send for ActivationPtr {}
// SAFETY: see `Send`; concurrent readers observe only immutable state.
unsafe impl Sync for ActivationPtr {}

impl ActivationPtr {
    /// Reclaims the strong reference the raw pointer has carried since
    /// construction.
    ///
    /// # Safety
    ///
    /// Call exactly once per pointer, only after the activation's `teardown`
    /// has returned (or after a failed `setup` that never published the
    /// vtable). A later vtable call through the copied `host_handle` is
    /// undefined behavior by contract.
    unsafe fn reclaim(self) {
        // SAFETY: per the caller contract, this is the unique reclaim of the
        // reference produced by the single `Arc::into_raw` at construction.
        drop(unsafe { Arc::from_raw(self.0.cast::<Activation>()) });
    }
}

/// Builds a byte slice from a vtable input pointer; null pairs with length 0
/// and yields an empty slice.
///
/// # Safety
///
/// When non-null, `data` must be readable for `len` bytes (the vtable's
/// caller-side contract).
const unsafe fn input_slice<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if data.is_null() {
        &[]
    } else {
        // SAFETY: the caller contract stated above.
        unsafe { std::slice::from_raw_parts(data, len) }
    }
}

/// Writes a JSON `{"code", "message"}` detail document into the caller's
/// out-buffer, then reports `status` — or [`STATUS_CAPACITY`] when the
/// detail does not fit, so the caller retries with a larger buffer.
fn write_detail(
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
    code: &str,
    message: &str,
    status: i32,
) -> i32 {
    let detail = serde_json::json!({ "code": code, "message": message });
    // Serializing a string-only JSON object cannot fail; if it ever did,
    // reporting the bare status stays contract-correct (detail is optional).
    let Ok(bytes) = serde_json::to_vec(&detail) else {
        return status;
    };
    // SAFETY: the host honors the same caller-owned buffer contract it
    // requires of plugin exports.
    if unsafe { write_out(&bytes, out_buf, cap, out_len) } {
        status
    } else {
        STATUS_CAPACITY
    }
}

/// Drives `future` to completion on the calling thread with the runtime
/// context entered (D-V3-4 baseline). Polling never leaves the calling
/// thread, which keeps the reentrancy stack exact on this path, works on
/// plugin-owned threads and runtime workers alike, and never depends on the
/// calling thread being inside tokio.
fn drive<T>(rt: &tokio::runtime::Handle, future: impl Future<Output = T>) -> T {
    let _runtime = rt.enter();
    block_on(future)
}

/// Minimal std-only `block_on`: parks the current thread until the future's
/// waker unparks it; spurious wakeups simply re-poll through the loop.
fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// vtable entry: one synchronous, blocking cross-plugin service call.
///
/// # Safety
///
/// Per the [`HostExports`] contract: `host` names a live activation,
/// `service`/`in_data` are readable for their lengths, and `out_buf`/
/// `out_len` follow the caller-owned buffer protocol.
unsafe extern "C" fn host_require(
    host: *mut std::ffi::c_void,
    service: *const u8,
    service_len: usize,
    method: u32,
    in_data: *const u8,
    in_len: usize,
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: the vtable contract keeps `host` valid until this activation's
    // teardown returns; the deref only reads shared immutable state.
    let activation = unsafe { &*host.cast::<Activation>() };
    // A host panic must never unwind across the FFI boundary (A4): contain it
    // and report a host error instead.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        require_impl(
            activation,
            service,
            service_len,
            method,
            in_data,
            in_len,
            out_buf,
            cap,
            out_len,
        )
    }))
    .unwrap_or_else(|_| {
        write_detail(
            out_buf,
            cap,
            out_len,
            "host_panic",
            "host require entry panicked",
            STATUS_HOST_ERROR,
        )
    })
}

/// The `require` bridge's prescribed steps: capability check, realm-scoped
/// resolution, dynamic-channel invocation, result write-back.
#[expect(
    clippy::too_many_arguments,
    reason = "the vtable signature is fixed by the ABI contract"
)]
fn require_impl(
    activation: &Activation,
    service: *const u8,
    service_len: usize,
    method: u32,
    in_data: *const u8,
    in_len: usize,
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: vtable input contract (null pairs with length 0).
    let service_bytes = unsafe { input_slice(service, service_len) };
    let Ok(service) = std::str::from_utf8(service_bytes) else {
        return write_detail(
            out_buf,
            cap,
            out_len,
            "undeclared_dependency",
            "service name is not valid UTF-8",
            STATUS_UNDECLARED,
        );
    };
    // Step 1: capability check against the descriptor `inject` set.
    if !activation.inject.iter().any(|declared| declared == service) {
        return write_detail(
            out_buf,
            cap,
            out_len,
            "undeclared_dependency",
            &format!("cannot get property \"{service}\" without inject"),
            STATUS_UNDECLARED,
        );
    }
    // Step 2: resolution in the activation's realm; `None` here means the
    // provider is not committed (step 1 already proved the declaration).
    let Some(invoker) = activation.ctx.get::<Invoker>(service) else {
        return write_detail(
            out_buf,
            cap,
            out_len,
            "inactive_dependency",
            &format!("cannot get required service \"{service}\" in inactive context"),
            STATUS_INACTIVE,
        );
    };
    // SAFETY: vtable input contract.
    let payload = unsafe { input_slice(in_data, in_len) }.to_vec();
    // Steps 3-5: the reentrancy check fires at the target's entry
    // (`DylibInvoke::call`) and surfaces as the `reentrancy_cycle` code;
    // the invocation is driven on this thread.
    match drive(&activation.rt, invoker.call(method, payload)) {
        Ok(bytes) => {
            // SAFETY: caller-owned buffer protocol.
            if unsafe { write_out(&bytes, out_buf, cap, out_len) } {
                STATUS_OK
            } else {
                STATUS_CAPACITY
            }
        }
        Err(err) => {
            let status = if err.code == "reentrancy_cycle" {
                STATUS_REENTRANCY
            } else {
                STATUS_FAILED
            };
            write_detail(out_buf, cap, out_len, err.code, &err.message, status)
        }
    }
}

/// vtable entry: publishes one event to the kernel bus, enqueue-and-return.
///
/// # Safety
///
/// Per the [`HostExports`] contract: `host` names a live activation and
/// `event`/`payload` are readable for their lengths.
unsafe extern "C" fn host_emit(
    host: *mut std::ffi::c_void,
    event: *const u8,
    event_len: usize,
    payload: *const u8,
    payload_len: usize,
) -> i32 {
    // SAFETY: see `host_require`.
    let activation = unsafe { &*host.cast::<Activation>() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        emit_impl(activation, event, event_len, payload, payload_len)
    }))
    .unwrap_or(STATUS_HOST_ERROR)
}

/// The `emit` bridge: validates the name and payload, then forwards to the
/// kernel's fire-and-forget `emit` (listener failures are logged, never
/// propagated — the caller neither expects nor receives them).
fn emit_impl(
    activation: &Activation,
    event: *const u8,
    event_len: usize,
    payload: *const u8,
    payload_len: usize,
) -> i32 {
    // SAFETY: vtable input contract.
    let event_bytes = unsafe { input_slice(event, event_len) };
    let Ok(event) = std::str::from_utf8(event_bytes) else {
        return STATUS_INVALID_EVENT;
    };
    // Bridges never touch the framework-reserved prefix (aligned with the
    // `on_dispatch` rejection rule).
    if event.starts_with("internal/") {
        return STATUS_INVALID_EVENT;
    }
    // SAFETY: vtable input contract.
    let payload_bytes = unsafe { input_slice(payload, payload_len) };
    let Ok(value) = serde_json::from_slice::<Value>(payload_bytes) else {
        return STATUS_INVALID_PAYLOAD;
    };
    // `Context::emit` spawns the dispatch; entering the captured runtime
    // makes that legal on plugin-owned threads too.
    let _runtime = activation.rt.enter();
    activation.ctx.emit(event, value);
    STATUS_OK
}

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

    fn setup(&self, host: &HostExports) -> Option<Handle> {
        // SAFETY: `host` points at a live `HostExports` for the duration of
        // the call (the v3 contract); the returned handle is owned, opaque
        // plugin state.
        let handle = unsafe { ((*self.table).setup)(host) };
        if handle.is_null() {
            None
        } else {
            Some(Handle(handle))
        }
    }

    /// One service call: payload on success; on [`STATUS_FAILED`] the
    /// callee's best-effort UTF-8 diagnostic rides the error message (v3
    /// contract; failure is final, never retried).
    fn call_full(&self, handle: &Handle, method: u32, payload: &[u8]) -> Result<Vec<u8>> {
        let table = self.table;
        let handle = handle.0;
        let op = format!("call(method={method})");
        let mut cap = 4096_usize;
        loop {
            let mut buf = vec![0_u8; cap];
            let mut len = 0_usize;
            // SAFETY: handle came from `setup` and was not yet torn down;
            // buffer contract as above.
            let status = unsafe {
                ((*table).call)(
                    handle,
                    method,
                    payload.as_ptr(),
                    payload.len(),
                    buf.as_mut_ptr(),
                    buf.len(),
                    &raw mut len,
                )
            };
            match status {
                STATUS_OK => return Ok(buf[..len].to_vec()),
                STATUS_CAPACITY => {
                    cap = cap.saturating_mul(2);
                    if cap > Self::MAX_OUTPUT_BYTES {
                        return Err(Error::msg(format!(
                            "dylib {op} output exceeds {} bytes",
                            Self::MAX_OUTPUT_BYTES
                        )));
                    }
                }
                STATUS_FAILED => {
                    let detail = String::from_utf8_lossy(&buf[..len]);
                    return Err(Error::new(
                        "service_error",
                        format!("dylib {op} failed: {detail}"),
                    ));
                }
                other => {
                    return Err(Error::msg(format!("dylib {op} failed with status {other}")));
                }
            }
        }
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
    instance_id: u64,
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
        let instance_id = self.instance_id;
        Box::pin(async move {
            // Reentrancy check before touching the lock (D-V3-3 b'): firing
            // means this thread is already executing inside this instance, so
            // awaiting its non-reentrant mutex would self-deadlock. On the
            // outbound-`require` path the whole future is polled on one
            // thread (`drive`), so the check is exact there.
            if entered_on_this_thread(instance_id) {
                return Err(Error::new(
                    "reentrancy_cycle",
                    "dylib service call would re-enter an instance already executing on this thread",
                ));
            }
            let _instance = lock.lock().await;
            // The guard opens after the last `.await` and closes before the
            // next one, keeping push/pop pairing on this thread.
            let _entry = EntryGuard::enter(instance_id);
            exports.call_full(&handle, method, &payload)
        })
    }
}

/// Builds the mode-aware bridge for one listened event (ABI v2 CPS, D14).
///
/// The instance lock spans the whole invocation including a deferred
/// waterfall continuation, so plugin state cannot observe interleaving; the
/// reentrancy guard covers each synchronous FFI segment separately, since a
/// guard must never cross an `.await` (task migration would break the
/// thread-local push/pop pairing).
fn event_bridge(
    exports: Exports,
    handle: Handle,
    lock: InstanceLock,
    event_id: u32,
    instance_id: u64,
) -> DispatchListener {
    Arc::new(move |_ctx, payload, next, mode| {
        let exports = exports.clone();
        let handle = handle.clone();
        let lock = Arc::clone(&lock);
        Box::pin(async move {
            let _instance = lock.lock().await;
            let bytes = serde_json::to_vec(&payload)
                .map_err(|err| Error::msg(format!("encode payload: {err}")))?;
            let waterfall = mode == DispatchMode::Waterfall;
            let (status, reply) = {
                let _entry = EntryGuard::enter(instance_id);
                exports.dispatch_event(
                    &handle,
                    event_id,
                    abi_mode(mode),
                    &bytes,
                    u32::from(waterfall),
                )?
            };
            match status {
                dougong_abi::STATUS_DEFERRED => {
                    if !waterfall {
                        return Err(Error::msg(format!(
                            "dylib plugin deferred event {event_id} outside waterfall"
                        )));
                    }
                    // DEFERRED output, when present, is the payload the chain
                    // continues with; empty output continues with the
                    // original.
                    let value: Value = if reply.is_empty() {
                        serde_json::from_slice(&bytes)
                            .map_err(|err| Error::msg(format!("decode payload: {err}")))?
                    } else {
                        serde_json::from_slice(&reply)
                            .map_err(|err| Error::msg(format!("decode payload: {err}")))?
                    };
                    let continued = next.call(value).await?;
                    let chain_bytes = serde_json::to_vec(&continued)
                        .map_err(|err| Error::msg(format!("encode payload: {err}")))?;
                    let processed = {
                        let _entry = EntryGuard::enter(instance_id);
                        exports.deliver_next(
                            &handle,
                            event_id,
                            dougong_abi::STATUS_OK,
                            &chain_bytes,
                        )?
                    };
                    serde_json::from_slice(&processed)
                        .map_err(|err| Error::msg(format!("decode payload: {err}")))
                }
                STATUS_FAILED => Err(Error::msg(format!(
                    "dylib on_event(event={event_id}) failed"
                ))),
                _ => serde_json::from_slice(&reply)
                    .map_err(|err| Error::msg(format!("decode payload: {err}"))),
            }
        })
    })
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
        let inject_names: Vec<String> = self
            .inject
            .iter()
            .map(|injection| injection.service.to_string())
            .collect();
        Box::pin(async move {
            let instance: InstanceLock = Arc::new(AsyncMutex::new(()));
            let instance_id = next_instance_id();
            // The outbound vtable is per-activation (A3): it captures this
            // fiber's context (realm-scoped resolution), the runtime handle
            // (foreign-thread driving), and the descriptor inject set
            // (capability basis). `apply` runs on a fiber task, so
            // `Handle::current` always succeeds here.
            let activation = ActivationPtr(
                Arc::into_raw(Arc::new(Activation {
                    ctx: ctx.clone(),
                    rt: tokio::runtime::Handle::current(),
                    inject: inject_names,
                }))
                .cast_mut()
                .cast::<std::ffi::c_void>(),
            );
            // Setup runs under the instance lock on the reentrancy stack
            // (uniform entry discipline); a fresh id cannot cycle. The
            // vtable is built inside the block: raw pointers are not `Send`
            // and must not be held across the `await` in a local.
            let setup = {
                let _instance = instance.lock().await;
                let _entry = EntryGuard::enter(instance_id);
                let host_exports = HostExports {
                    host_handle: activation.0,
                    require: host_require,
                    emit: host_emit,
                };
                exports.setup(&host_exports)
            };
            let Some(handle) = setup else {
                // SAFETY: setup failed, the vtable was never published; this
                // is the unique reclaim of the raw activation reference.
                unsafe { activation.reclaim() };
                return Err(Error::msg(format!(
                    "dylib plugin \"{plugin_name}\" setup returned null"
                )));
            };

            // Teardown runs exactly once per successful setup, after every
            // other effect (provide disposers, bridges) has drained — LIFO
            // places it first in the queue because it is registered here,
            // before the effects below. It also reclaims the activation:
            // vtable calls after teardown returns are outside the contract.
            let teardown_exports = exports.clone();
            let teardown_handle = Handle(handle.0);
            let teardown_lock = Arc::clone(&instance);
            let teardown_activation = activation;
            let disposer: dougong::Disposer = Box::new(move || {
                Box::pin(async move {
                    {
                        let _instance = teardown_lock.lock().await;
                        let _entry = EntryGuard::enter(instance_id);
                        teardown_exports.teardown(&teardown_handle);
                    }
                    // SAFETY: teardown has returned, so the vtable is dead;
                    // this is the unique reclaim of the activation reference.
                    unsafe { teardown_activation.reclaim() };
                })
            });
            ctx.effect("dylib.teardown", async move { Ok(Some(disposer)) })
                .await?;

            // Mode-aware bridges: one per declared listened event.
            for (event_name, event_id) in &listen {
                let bridge = event_bridge(
                    exports.clone(),
                    Handle(handle.0),
                    Arc::clone(&instance),
                    *event_id,
                    instance_id,
                );
                ctx.on_dispatch(event_name, bridge).await?;
            }

            for service in &provide {
                let invoker_exports = exports.clone();
                let invoker_handle = handle.0;
                let invoker = dougong::Invoker::new(Arc::new(DylibInvoke {
                    exports: invoker_exports,
                    handle: Handle(invoker_handle),
                    lock: Arc::clone(&instance),
                    instance_id,
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
mod outbound_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use dougong::Context;
    use dougong_abi::STATUS_DEFERRED;
    use serde_json::json;

    const FAKE_NAME: &[u8] = b"fake-plugin";

    unsafe extern "C" fn fake_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        // SAFETY: host buffer contract.
        if unsafe { write_out(FAKE_NAME, out, cap, len) } {
            STATUS_OK
        } else {
            STATUS_CAPACITY
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
            STATUS_CAPACITY
        }
    }

    unsafe extern "C" fn fake_setup(_host: *const HostExports) -> *mut std::os::raw::c_void {
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
        let outcome = plugin
            .exports
            .call_full(&Handle(std::ptr::null_mut()), 0, b"hi");
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
        let outcome = plugin
            .exports
            .call_full(&Handle(std::ptr::null_mut()), 0, b"");
        assert_eq!(outcome.expect("retry succeeds"), b"second");
    }

    #[test]
    fn entry_stack_pairs_and_detects_membership() {
        let id = next_instance_id();
        assert!(!entered_on_this_thread(id));
        {
            let _entry = EntryGuard::enter(id);
            assert!(entered_on_this_thread(id));
            {
                let other = EntryGuard::enter(next_instance_id());
                assert!(entered_on_this_thread(id));
                drop(other);
            }
            assert!(entered_on_this_thread(id));
        }
        assert!(!entered_on_this_thread(id));
    }

    #[tokio::test]
    async fn drive_runs_futures_on_foreign_threads() {
        let rt = tokio::runtime::Handle::current();
        // The sender lives on a plain thread, not a runtime task: the driven
        // future cannot complete without a genuine park/wake cycle, and the
        // test's runtime thread is never blocked on `join` while a runtime
        // task is still pending (which would deadlock a current-thread
        // runtime).
        let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _dropped = tx.send(41);
        });
        let outcome = std::thread::spawn(move || {
            drive(&rt, async move {
                let value = rx.await.expect("sender alive");
                value + 1
            })
        })
        .join()
        .expect("driver thread did not panic");
        assert_eq!(outcome, 42);
    }
}
