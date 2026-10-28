// Rust guideline compliant 2026-10-28
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
//! # Outbound surface (ABI v4)
//!
//! Each activation receives a [`dougong_abi::HostExports`] vtable at `setup`:
//! `require` performs a synchronous blocking call into another plugin's
//! service (capability-checked against the descriptor `inject` set, resolved
//! in the activation's realm), and `emit` publishes a JSON event to the
//! kernel bus. `grant` (D31) additionally lets a provider authorize one
//! committed target activation to require one owned service without static
//! injection; grants bind activation identities and are reclaimed with the
//! granter. Reentrancy is governed by a per-thread activation stack
//! (design decision D-V3-3 option b'): any foreign entry — `setup`, `call`,
//! `on_event`/`on_next`, `teardown` — records its instance for the duration
//! of the synchronous FFI segment, and a service call that would re-enter an
//! instance already executing on the calling thread fails deterministically
//! with `reentrancy_cycle` instead of deadlocking the per-instance lock.
//!
//! Since the 2026-10-25 amendment of D-V3-3 the same guarantee covers
//! cross-instance require chains: a held instance lock is visible to the
//! whole nested require chain, parking on a foreign instance lock arms a
//! wait-for edge from every held instance, and a chain whose wait edges
//! close a cycle fails with `reentrancy_cycle` on the thread that would
//! complete it. The per-instance lock can therefore never deadlock, no
//! matter which threads hold which instances.
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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
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
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

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

std::thread_local! {
    /// Instances whose per-instance locks the current execution context holds
    /// right now, outermost first. This is the held side of the cross-instance
    /// reentrancy policy (D-V3-3, 2026-10-25 amendment): a foreign entry pushes
    /// its instance after acquiring the lock, a nested `require` arms a wait
    /// edge from every held instance, and a closed wait cycle fails
    /// deterministically instead of deadlocking. Guards never span `.await`
    /// points; the one lock hold that does — a deferred waterfall continuation —
    /// re-establishes its chain through [`ChainPoll`] on every poll instead.
    static LOCK_CHAIN: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Snapshot of the instance locks held by the current execution context.
fn chain_held() -> Vec<u64> {
    LOCK_CHAIN.with(|chain| chain.borrow().clone())
}

/// RAII guard: pushes `id` on creation, pops it on drop. Like
/// [`EntryGuard`], it must never span an `.await` — task migration breaks
/// the thread-local push/pop pairing; [`ChainPoll`] carries the chain across
/// the one await-spanning lock hold instead.
struct ChainGuard(u64);

impl ChainGuard {
    fn hold(id: u64) -> Self {
        LOCK_CHAIN.with(|chain| chain.borrow_mut().push(id));
        Self(id)
    }
}

impl Drop for ChainGuard {
    fn drop(&mut self) {
        LOCK_CHAIN.with(|chain| {
            let popped = chain.borrow_mut().pop();
            debug_assert_eq!(popped, Some(self.0), "lock chain pairing broken");
        });
    }
}

/// Wait edges of the cross-instance wait-for graph: `held instance -> target
/// instance` while the holder of `held` is parked acquiring `target`. Keys
/// are process-unique instance ids ([`next_instance_id`] never repeats), so
/// entries can never alias across activations.
static WAIT_EDGES: LazyLock<Mutex<HashMap<u64, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Test-only snapshot: true when the wait graph holds no edges.
#[cfg(test)]
fn wait_edges_empty() -> bool {
    WAIT_EDGES.lock().is_empty()
}

/// The wait edges armed by one parking foreign entry: alive from the moment
/// the edges are inserted until the lock is granted — or until the parking
/// future is cancelled, whose drop path keeps the graph exact as well.
#[derive(Debug)]
struct WaitToken {
    held: Vec<u64>,
    target: u64,
}

impl Drop for WaitToken {
    fn drop(&mut self) {
        if self.held.is_empty() {
            return; // fast path: nothing was armed
        }
        let mut edges = WAIT_EDGES.lock();
        for held in &self.held {
            if edges.get(held) == Some(&self.target) {
                edges.remove(held);
            }
        }
    }
}

/// Arms the wait edges for a foreign entry about to park on `target`'s
/// instance lock, and fails deterministically when parking would close a
/// wait cycle. Every instance waits for at most one target, so the graph's
/// out-degree is at most one: walking forward from `target` either
/// terminates or revisits a node, and a revisit means the parking would
/// deadlock. The returned token must stay alive across the `lock().await`
/// and drop as soon as the lock is granted.
///
/// # Errors
///
/// `reentrancy_cycle` naming the closed chain `target -> ... -> held`, when
/// the graph already contains a wait path from `target` back into `held`.
fn arm_wait_edges(held: Vec<u64>, target: u64) -> Result<WaitToken, Error> {
    if held.is_empty() {
        // No held lock can be part of a cycle: parking cannot deadlock.
        return Ok(WaitToken { held, target });
    }
    let mut edges = WAIT_EDGES.lock();
    for held_id in &held {
        edges.insert(*held_id, target);
    }
    // The walk is deterministic and cheap at chain depth; under sound
    // bookkeeping a revisit is always `target` itself, and a foreign cycle
    // (never insertable) would be treated the same — parking into one would
    // deadlock all the same.
    let mut path = vec![target];
    let mut cursor = target;
    let mut closer: Option<u64> = None;
    let deadlock = loop {
        match edges.get(&cursor) {
            None => break false,
            Some(&next) => {
                if next == target || path.contains(&next) {
                    closer = Some(next);
                    break true;
                }
                path.push(next);
                cursor = next;
            }
        }
    };
    if !deadlock {
        return Ok(WaitToken { held, target });
    }
    // The walk stops on the revisited node; repeat it to render the closed
    // cycle ("target -> ... -> target").
    if let Some(last) = closer {
        path.push(last);
    }
    let chain = path
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(" -> ");
    drop(edges); // the token's drop re-locks the graph
    drop(WaitToken { held, target });
    Err(Error::new(
        "reentrancy_cycle",
        format!("cross-instance require chain would deadlock: {chain}"),
    ))
}

/// Lock guard paired with its lock-chain guard. The tuple drops fields in
/// order, so the chain pops *before* the lock releases — the graph never
/// names a released lock. Field order is load-bearing.
type InstanceHold = (ChainGuard, OwnedMutexGuard<()>);

/// Acquires one instance lock under the full cross-instance discipline: wait
/// edges armed while parking, the instance pushed on the lock chain while
/// held. A fresh execution context pays only the empty-chain fast path.
///
/// # Errors
///
/// `reentrancy_cycle` when the wait graph shows the parking would close a
/// cycle (the caller's held locks loop back into `target`).
async fn enter_exclusive(lock: &InstanceLock, instance_id: u64) -> Result<InstanceHold, Error> {
    let token = arm_wait_edges(chain_held(), instance_id)?;
    let guard = Arc::clone(lock).lock_owned().await;
    drop(token);
    Ok((ChainGuard::hold(instance_id), guard))
}

/// Re-establishes a lock chain on every poll of `fut`. A deferred waterfall
/// continuation is awaited *while its bridge still holds the instance lock*;
/// a guard cannot span that `.await` (task migration breaks thread-local
/// pairing), so the held chain travels inside the wrapper and is pushed and
/// popped around each poll — the chain stays visible to nested acquisitions
/// no matter which thread resumes the continuation.
struct ChainPoll<F> {
    held: Vec<u64>,
    fut: F,
}

impl<F: Future + Unpin> Future for ChainPoll<F> {
    type Output = F::Output;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        struct Release<'a>(&'a [u64]);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                LOCK_CHAIN.with(|chain| {
                    let mut chain = chain.borrow_mut();
                    let keep = chain.len().saturating_sub(self.0.len());
                    debug_assert_eq!(&chain[keep..], self.0, "chain extension pairing broken");
                    chain.truncate(keep);
                });
            }
        }
        let Self { held, fut } = self.get_mut();
        LOCK_CHAIN.with(|chain| chain.borrow_mut().extend_from_slice(held));
        let _release = Release(held);
        std::pin::Pin::new(fut).poll(cx)
    }
}

/// Host-side state of one plugin activation backing the v4 [`HostExports`]
/// vtable: built per activation, dead once that activation's `teardown`
/// returns (the vtable's documented lifetime bound).
struct Activation {
    /// The owning fiber's context: capability resolution and event dispatch
    /// run through it, so realm binding matches the in-process form (D15).
    ctx: dougong::Context,
    /// Runtime captured during `apply`; `enter`ing it gives foreign threads
    /// (plugin-owned, never inside tokio) a valid runtime context.
    rt: tokio::runtime::Handle,
    /// Process-unique identity of this plugin activation (D31).
    instance_id: u64,
    /// Descriptor `inject` names: the constant capability-check basis for
    /// outbound `require` (the in-process form uses the fiber interest set;
    /// the boundary form uses the descriptor set — see the equivalence table
    /// in `doc/research/dougong-design.md`).
    inject: Vec<String>,
    /// Descriptor `provide` names: the only services this activation may
    /// grant to another activation (D31).
    provide: Vec<String>,
}

/// One activation-granular grant. The resolved target Arc is retained for
/// identity stability and idempotency; it is never called through the table.
struct GrantEntry {
    granter: u64,
    target_activation: Option<u64>,
    target: Arc<dyn dougong::Invoke>,
}

/// Process-wide grants indexed by granted service name (D31). Absence of the
/// map key is the zero-resolution name-index miss required by C1. Instance
/// ids are globally unique, so independent kernel roots cannot alias.
static GRANTS: LazyLock<Mutex<HashMap<String, Vec<GrantEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
std::thread_local! {
    /// Per-thread probe for G7: static require hits never call the lookup.
    static GRANT_LOOKUPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn grant_lookups_on_this_thread() -> u64 {
    GRANT_LOOKUPS.with(std::cell::Cell::get)
}

enum GrantLookup {
    Missing,
    Present { authorized: bool },
}

/// Looks up one caller's capability without holding the lock across kernel
/// resolution, a foreign call, an instance lock, or logging (C4).
fn lookup_grant(activation: &Activation, service: &str) -> GrantLookup {
    #[cfg(test)]
    GRANT_LOOKUPS.with(|count| count.set(count.get() + 1));
    GRANTS
        .lock()
        .get(service)
        .map_or(GrantLookup::Missing, |entries| GrantLookup::Present {
            authorized: entries
                .iter()
                .any(|entry| entry.target_activation == Some(activation.instance_id)),
        })
}

#[cfg(test)]
fn grant_entries(service: &str) -> Vec<(u64, Option<u64>)> {
    GRANTS.lock().get(service).map_or_else(Vec::new, |entries| {
        entries
            .iter()
            .map(|entry| (entry.granter, entry.target_activation))
            .collect()
    })
}

/// Removes one granter activation's entries and drops their target Arcs only
/// after releasing the table lock. A new activation never inherits them.
fn clear_grants(activation: &Activation) {
    let mut removed = Vec::new();
    {
        let mut grants = GRANTS.lock();
        for entries in grants.values_mut() {
            let mut index = 0;
            while index < entries.len() {
                if entries[index].granter == activation.instance_id {
                    removed.push(entries.swap_remove(index));
                } else {
                    index += 1;
                }
            }
        }
        grants.retain(|_, entries| !entries.is_empty());
    }
    if removed.is_empty() {
        return;
    }
    activation.ctx.logger().log_record(
        &dougong::LogRecord::new(
            dougong::LogLevel::Info,
            "dylib",
            "outbound_grant",
            format!("cleared {} grant(s)", removed.len()),
        )
        .with_trace(Some(activation.ctx.trace_id()))
        .with_boundary(Some("dylib")),
    );
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
        let activation = unsafe { Arc::from_raw(self.0.cast::<Activation>()) };
        clear_grants(&activation);
        drop(activation);
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

/// Writes a JSON `{"code", "class", "message"}` detail document into the
/// caller's out-buffer, then reports `status` — or [`STATUS_CAPACITY`] when
/// the detail does not fit, so the caller retries with a larger buffer.
///
/// `class` is the kernel's S5 error class (`logic` / `timeout` /
/// `transport`): it makes the error's type explicit to the plugin without
/// growing the status-code set (D20).
fn write_detail(
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
    code: &str,
    class: &str,
    message: &str,
    status: i32,
) -> i32 {
    let detail = serde_json::json!({ "code": code, "class": class, "message": message });
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
            dougong::ErrorClass::Transport.as_str(),
            "host require entry panicked",
            STATUS_HOST_ERROR,
        )
    })
}

/// Writes the legacy undeclared-require response verbatim (G2/C3).
fn undeclared_require(service: &str, out_buf: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    write_detail(
        out_buf,
        cap,
        out_len,
        "undeclared_dependency",
        dougong::ErrorClass::Logic.as_str(),
        &format!("cannot get property \"{service}\" without inject"),
        STATUS_UNDECLARED,
    )
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
            dougong::ErrorClass::Logic.as_str(),
            "service name is not valid UTF-8",
            STATUS_UNDECLARED,
        );
    };
    // Step 1: descriptor injection bypasses the grant table entirely (G7).
    // A static miss consults the name index; an absent name is the legacy
    // undeclared path with byte-identical detail (C1/C3).
    let authorized = if activation.inject.iter().any(|declared| declared == service) {
        true
    } else {
        match lookup_grant(activation, service) {
            GrantLookup::Missing => {
                return undeclared_require(service, out_buf, cap, out_len);
            }
            GrantLookup::Present { authorized } => authorized,
        }
    };
    // Step 2: resolution in the activation's realm. Resolution precedes the
    // unauthorized rejection for grant-known names, preserving INACTIVE for
    // an absent provider and UNDECLARED for a committed one (C3).
    let Some(invoker) = activation.ctx.get::<Invoker>(service) else {
        return write_detail(
            out_buf,
            cap,
            out_len,
            "inactive_dependency",
            dougong::ErrorClass::Logic.as_str(),
            &format!("cannot get required service \"{service}\" in inactive context"),
            STATUS_INACTIVE,
        );
    };
    if !authorized {
        return undeclared_require(service, out_buf, cap, out_len);
    }
    // SAFETY: vtable input contract.
    let payload = unsafe { input_slice(in_data, in_len) }.to_vec();
    let bytes_in = payload.len();
    // Steps 3-5: the reentrancy check fires at the target's entry
    // (`DylibInvoke::call`) and surfaces as the `reentrancy_cycle` code;
    // the invocation is driven on this thread.
    // O7: the outbound leg of a cross-boundary call carries the guest
    // fiber's own identity (realm logger, trace id) and the call outcome.
    let started = std::time::Instant::now();
    let outcome = drive(&activation.rt, invoker.call(method, payload));
    let duration = started.elapsed();
    let record = match &outcome {
        Ok(bytes) => dougong::LogRecord::new(
            dougong::LogLevel::Info,
            "dylib",
            "outbound_call",
            format!(
                "require {service}#{method} bytes_in={bytes_in} bytes_out={} ok",
                bytes.len()
            ),
        )
        .with_service(service)
        .with_trace(Some(activation.ctx.trace_id()))
        .with_boundary(Some("dylib"))
        .with_duration(duration),
        Err(err) => dougong::LogRecord::new(
            dougong::LogLevel::Error,
            "dylib",
            "outbound_call",
            format!("require {service}#{method} bytes_in={bytes_in} failed"),
        )
        .with_service(service)
        .with_trace(Some(activation.ctx.trace_id()))
        .with_boundary(Some("dylib"))
        .with_duration(duration)
        .with_cause(err.to_string()),
    };
    activation.ctx.logger().log_record(&record);
    match outcome {
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
            write_detail(
                out_buf,
                cap,
                out_len,
                err.code,
                err.class.as_str(),
                &err.message,
                status,
            )
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

/// Internal grant failure: the C entry returns only `status` because the v4
/// signature deliberately has no out-buffer; tests retain the detail code.
#[derive(Debug)]
struct GrantFailure {
    code: &'static str,
    message: String,
    status: i32,
}

impl GrantFailure {
    fn invalid_name() -> Self {
        Self {
            code: "invalid_grant_name",
            message: "grant service names must be valid UTF-8".to_string(),
            status: STATUS_FAILED,
        }
    }
}

/// Registers one narrow capability grant without invoking either activation.
fn grant_impl(
    activation: &Activation,
    target_service: &[u8],
    granted_service: &[u8],
) -> std::result::Result<(), GrantFailure> {
    let (Ok(target_service), Ok(granted_service)) = (
        std::str::from_utf8(target_service),
        std::str::from_utf8(granted_service),
    ) else {
        return Err(GrantFailure::invalid_name());
    };
    if !activation
        .provide
        .iter()
        .any(|provided| provided == granted_service)
    {
        return Err(GrantFailure {
            code: "grant_not_owned",
            message: format!("cannot grant service \"{granted_service}\" without provide"),
            status: STATUS_FAILED,
        });
    }
    // Resolution is fail-closed and uses the same realm path as require. It
    // happens outside the grant lock and never invokes the target (A3/B2).
    let Some(target) = activation.ctx.get::<Invoker>(target_service) else {
        return Err(GrantFailure {
            code: "inactive_dependency",
            message: format!(
                "cannot get required service \"{target_service}\" in inactive context"
            ),
            status: STATUS_INACTIVE,
        });
    };
    let target_activation = target.inner.activation_id();
    let mut grants = GRANTS.lock();
    let entries = grants.entry(granted_service.to_string()).or_default();
    let duplicate = entries.iter().any(|entry| {
        entry.granter == activation.instance_id
            && match (entry.target_activation, target_activation) {
                (Some(left), Some(right)) => left == right,
                (None, None) => Arc::ptr_eq(&entry.target, &target.inner),
                _ => false,
            }
    });
    if !duplicate {
        entries.push(GrantEntry {
            granter: activation.instance_id,
            target_activation,
            target: Arc::clone(&target.inner),
        });
    }
    drop(grants);
    // Every successful grant, including an idempotent re-grant, is audited
    // after unlocking. The target name is in the message; service is granted.
    activation.ctx.logger().log_record(
        &dougong::LogRecord::new(
            dougong::LogLevel::Info,
            "dylib",
            "outbound_grant",
            format!("grant {granted_service} target={target_service} ok"),
        )
        .with_service(granted_service)
        .with_trace(Some(activation.ctx.trace_id()))
        .with_boundary(Some("dylib")),
    );
    Ok(())
}

/// vtable entry: grants one committed target activation a require capability.
///
/// # Safety
///
/// Per the [`HostExports`] contract: `host` names a live activation and both
/// service names are readable for their lengths.
unsafe extern "C" fn host_grant(
    host: *mut std::ffi::c_void,
    target_service: *const u8,
    target_service_len: usize,
    granted_service: *const u8,
    granted_service_len: usize,
) -> i32 {
    // SAFETY: the vtable contract keeps `host` valid until teardown returns.
    let activation = unsafe { &*host.cast::<Activation>() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: vtable input contract (null pairs with length 0).
        let target = unsafe { input_slice(target_service, target_service_len) };
        // SAFETY: same input contract for the granted name.
        let granted = unsafe { input_slice(granted_service, granted_service_len) };
        match grant_impl(activation, target, granted) {
            Ok(()) => STATUS_OK,
            Err(err) => {
                activation.ctx.logger().log_record(
                    &dougong::LogRecord::new(
                        dougong::LogLevel::Error,
                        "dylib",
                        "outbound_grant",
                        format!("grant rejected: {}", err.message),
                    )
                    .with_trace(Some(activation.ctx.trace_id()))
                    .with_boundary(Some("dylib"))
                    .with_cause(err.code),
                );
                err.status
            }
        }
    }))
    .unwrap_or(STATUS_HOST_ERROR)
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
                        return Err(Error::transport(
                            "ffi_status",
                            format!("dylib {op} output exceeds {} bytes", Self::MAX_OUTPUT_BYTES),
                        ));
                    }
                }
                other => {
                    return Err(Error::transport(
                        "ffi_status",
                        format!("dylib {op} failed with status {other}"),
                    ));
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
        serde_json::from_slice(&bytes).map_err(|err| {
            Error::transport(
                "plugin_load",
                format!("dylib descriptor is not valid JSON: {err}"),
            )
        })
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
                        return Err(Error::transport(
                            "ffi_status",
                            format!("dylib {op} output exceeds {} bytes", Self::MAX_OUTPUT_BYTES),
                        ));
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
                    return Err(Error::transport(
                        "ffi_status",
                        format!("dylib {op} failed with status {other}"),
                    ));
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
                        return Err(Error::transport(
                            "ffi_status",
                            format!(
                                "dylib on_event(event={event}) output exceeds {} bytes",
                                Self::MAX_OUTPUT_BYTES
                            ),
                        ));
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
                        return Err(Error::transport(
                            "ffi_status",
                            format!(
                                "dylib on_next(event={event}) output exceeds {} bytes",
                                Self::MAX_OUTPUT_BYTES
                            ),
                        ));
                    }
                }
                dougong_abi::STATUS_OK => return Ok(buf[..len].to_vec()),
                dougong_abi::STATUS_DEFERRED => {
                    return Err(Error::transport(
                        "ffi_status",
                        "dylib on_next returned STATUS_DEFERRED, which is a contract violation",
                    ));
                }
                other => {
                    return Err(Error::transport(
                        "ffi_status",
                        format!("dylib on_next(event={event}) failed with status {other}"),
                    ));
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
    fn activation_id(&self) -> Option<u64> {
        Some(self.instance_id)
    }

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
            // Cross-instance discipline: wait edges while parking, this
            // instance on the lock chain while held — a `require` issued
            // from inside the handler must see this lock as held.
            let _hold = enter_exclusive(&lock, instance_id).await?;
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
            // The bridge takes the instance lock under the full
            // cross-instance discipline (same as `call`): wait edges while
            // parking, the instance on the lock chain while held.
            let hold = enter_exclusive(&lock, instance_id).await?;
            let bytes = serde_json::to_vec(&payload).map_err(|err| {
                Error::transport("payload_codec", format!("encode payload: {err}"))
            })?;
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
                        return Err(Error::transport(
                            "ffi_status",
                            format!("dylib plugin deferred event {event_id} outside waterfall"),
                        ));
                    }
                    // DEFERRED output, when present, is the payload the chain
                    // continues with; empty output continues with the
                    // original.
                    let value: Value = if reply.is_empty() {
                        serde_json::from_slice(&bytes).map_err(|err| {
                            Error::transport("payload_codec", format!("decode payload: {err}"))
                        })?
                    } else {
                        serde_json::from_slice(&reply).map_err(|err| {
                            Error::transport("payload_codec", format!("decode payload: {err}"))
                        })?
                    };
                    // The lock keeps spanning the whole invocation, so the
                    // continuation polls must keep seeing this instance as a
                    // held lock — a guard cannot span the `.await`, so
                    // `ChainPoll` re-establishes the chain per poll.
                    drop(hold.0);
                    let mut chain = chain_held();
                    chain.push(instance_id);
                    let continued = ChainPoll {
                        held: chain,
                        fut: next.call(value),
                    }
                    .await?;
                    let _chain = ChainGuard::hold(instance_id);
                    let chain_bytes = serde_json::to_vec(&continued).map_err(|err| {
                        Error::transport("payload_codec", format!("encode payload: {err}"))
                    })?;
                    let processed = {
                        let _entry = EntryGuard::enter(instance_id);
                        exports.deliver_next(
                            &handle,
                            event_id,
                            dougong_abi::STATUS_OK,
                            &chain_bytes,
                        )?
                    };
                    serde_json::from_slice(&processed).map_err(|err| {
                        Error::transport("payload_codec", format!("decode payload: {err}"))
                    })
                }
                STATUS_FAILED => Err(Error::new(
                    "listener_failed",
                    format!("dylib on_event(event={event_id}) failed"),
                )),
                _ => serde_json::from_slice(&reply).map_err(|err| {
                    Error::transport("payload_codec", format!("decode payload: {err}"))
                }),
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

    fn boundary_tag(&self) -> Option<&'static str> {
        Some("dylib")
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
                    instance_id,
                    inject: inject_names,
                    provide: provide.clone(),
                }))
                .cast_mut()
                .cast::<std::ffi::c_void>(),
            );
            // Setup runs under the instance lock on the reentrancy stack
            // (uniform entry discipline); a fresh id cannot cycle. The
            // vtable is built inside the block: raw pointers are not `Send`
            // and must not be held across the `await` in a local.
            let setup = {
                let _hold = enter_exclusive(&instance, instance_id).await?;
                let _entry = EntryGuard::enter(instance_id);
                let host_exports = HostExports {
                    host_handle: activation.0,
                    require: host_require,
                    emit: host_emit,
                    grant: host_grant,
                };
                exports.setup(&host_exports)
            };
            let Some(handle) = setup else {
                // SAFETY: setup failed, the vtable was never published; this
                // is the unique reclaim of the raw activation reference.
                unsafe { activation.reclaim() };
                return Err(Error::new(
                    "setup_failed",
                    format!("dylib plugin \"{plugin_name}\" setup returned null"),
                ));
            };

            // Teardown runs exactly once per successful setup, after every
            // other effect (provide disposers, bridges) has drained — LIFO
            // places it first in the queue because it is registered here,
            // before the effects below. It also reclaims the activation:
            // vtable calls after teardown returns are outside the contract.
            // SAFETY-shared token: exactly one of the disposer and the inline
            // fallback below ever runs, so exactly one copy reclaims; keeping
            // the copy in the wrapper (not the raw pointer) keeps the apply
            // future `Send` across the effect registration await.
            let fallback_activation = ActivationPtr(activation.0);
            let teardown_exports = exports.clone();
            let teardown_handle = Handle(handle.0);
            let teardown_lock = Arc::clone(&instance);
            let teardown_activation = activation;
            let disposer: dougong::Disposer = Box::new(move || {
                Box::pin(async move {
                    {
                        // Teardown acquires exactly one lock and holds
                        // nothing else while parking, so it can never close a
                        // wait cycle — plain parking is safe; the chain push
                        // still exposes this lock to nested `require` arms.
                        let _instance = teardown_lock.lock().await;
                        let _chain = ChainGuard::hold(instance_id);
                        let _entry = EntryGuard::enter(instance_id);
                        teardown_exports.teardown(&teardown_handle);
                    }
                    // SAFETY: teardown has returned, so the vtable is dead;
                    // this is the unique reclaim of the activation reference.
                    unsafe { teardown_activation.reclaim() };
                })
            });
            if let Err(err) = ctx
                .effect("dylib.teardown", async move { Ok(Some(disposer)) })
                .await
            {
                // The effect was never registered, so the disposer above (and
                // with it the plugin's `teardown` and the activation reclaim)
                // would silently never run. Tear down inline instead: the
                // failure arms are a dispose racing this body or an effect
                // budget breach, and in both the activation is dead weight.
                {
                    // Same one-lock shape as the disposer: no wait edges
                    // needed (nothing else is held), but the chain push keeps
                    // this lock visible to nested `require` arms.
                    let _instance = instance.lock().await;
                    let _chain = ChainGuard::hold(instance_id);
                    let _entry = EntryGuard::enter(instance_id);
                    exports.teardown(&Handle(handle.0));
                }
                // SAFETY: the disposer was never registered, so this is the
                // unique reclaim of the activation reference.
                unsafe { fallback_activation.reclaim() };
                return Err(err);
            }

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
    let lib = unsafe { Library::new(path) }.map_err(|err| {
        Error::transport("plugin_load", format!("dlopen {}: {err}", path.display()))
    })?;
    let lib = Arc::new(lib);
    // SAFETY: the symbol lives in the just-mapped library, which the
    // `Arc<Library>` kept alongside keeps alive.
    let table: libloading::Symbol<*const PluginExports> =
        unsafe { lib.get(b"DOUGONG_PLUGIN_EXPORTS") }.map_err(|err| {
            Error::transport(
                "plugin_load",
                format!("missing DOUGONG_PLUGIN_EXPORTS: {err}"),
            )
        })?;
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
        return Err(Error::transport(
            "plugin_load",
            format!(
                "ABI version mismatch: plugin exports {}, host expects {ABI_VERSION}",
                exports.abi_version()
            ),
        ));
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
                    return Err(Error::transport(
                        "plugin_load",
                        format!("descriptor listen[\"{event_name}\"] must be a u32 event id"),
                    ));
                };
                listen.push((event_name.clone(), id));
            }
            listen
        }
        Some(_) => {
            return Err(Error::transport(
                "plugin_load",
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

/// Adapts a statically-resolved export table: tests, demos, and hosts that
/// embed their tables in the binary. Runs the same ABI-gate and descriptor
/// checks as [`load`].
///
/// # Errors
///
/// Fails when the ABI version mismatches or the descriptor is malformed.
pub fn from_static(table: &'static PluginExports) -> Result<DynPlugin> {
    from_exports(Exports { table, _lib: None })
}

#[cfg(test)]
mod fault_tests;
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
        // S5: artifact loading is a boundary-transport failure.
        assert_eq!(err.code, "plugin_load");
        assert_eq!(err.class, dougong::ErrorClass::Transport);
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
        assert_eq!(err.code, "plugin_load");
        assert_eq!(err.class, dougong::ErrorClass::Transport);
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

    unsafe extern "C" fn failing_call(
        _handle: *mut std::os::raw::c_void,
        _method: u32,
        _data: *const u8,
        _len: usize,
        out: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32 {
        // SAFETY: host buffer contract; the detail is the callee's business
        // diagnostic (v3 contract).
        if unsafe { write_out(b"bad name", out, cap, out_len) } {
            dougong_abi::STATUS_FAILED
        } else {
            dougong_abi::STATUS_CAPACITY
        }
    }

    unsafe extern "C" fn weird_call(
        _handle: *mut std::os::raw::c_void,
        _method: u32,
        _data: *const u8,
        _len: usize,
        _out: *mut u8,
        _cap: usize,
        _out_len: *mut usize,
    ) -> i32 {
        // Not a status the v3 contract defines: the boundary itself misbehaves.
        42
    }

    static FAILING_TABLE: PluginExports = PluginExports {
        abi_version: ABI_VERSION,
        name: fake_name,
        descriptor: fake_descriptor,
        setup: fake_setup,
        call: failing_call,
        on_event: fake_on_event,
        on_next: fake_on_next,
        teardown: fake_teardown,
    };

    static WEIRD_TABLE: PluginExports = PluginExports {
        abi_version: ABI_VERSION,
        name: fake_name,
        descriptor: fake_descriptor,
        setup: fake_setup,
        call: weird_call,
        on_event: fake_on_event,
        on_next: fake_on_next,
        teardown: fake_teardown,
    };

    #[test]
    fn call_statuses_map_to_classes() {
        // STATUS_FAILED carried a business diagnostic: the transport worked,
        // so the failure is the callee's logic.
        let plugin = from_static(&FAILING_TABLE).expect("adapter builds");
        let err = plugin
            .exports
            .call_full(&Handle(std::ptr::null_mut()), 0, b"")
            .expect_err("business failure");
        assert_eq!(err.code, "service_error");
        assert_eq!(err.class, dougong::ErrorClass::Logic);
        assert!(err.message.contains("bad name"));
        // An undefined status is a boundary-transport failure.
        let plugin = from_static(&WEIRD_TABLE).expect("adapter builds");
        let err = plugin
            .exports
            .call_full(&Handle(std::ptr::null_mut()), 0, b"")
            .expect_err("unknown status");
        assert_eq!(err.code, "ffi_status");
        assert_eq!(err.class, dougong::ErrorClass::Transport);
    }

    #[test]
    fn outbound_detail_carries_the_class_field() {
        let mut buf = [0_u8; 256];
        let mut len = 0_usize;
        let status = write_detail(
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut len,
            "wasm_trap",
            dougong::ErrorClass::Transport.as_str(),
            "trap",
            STATUS_FAILED,
        );
        assert_eq!(status, STATUS_FAILED);
        let detail: Value = serde_json::from_slice(&buf[..len]).expect("detail is JSON");
        assert_eq!(detail["code"], json!("wasm_trap"));
        assert_eq!(detail["class"], json!("transport"));
        assert_eq!(detail["message"], json!("trap"));
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

#[cfg(test)]
mod wait_graph_tests {
    use super::*;

    #[test]
    fn arms_edges_for_a_parking_wait_and_releases_on_drop() {
        let held = next_instance_id();
        let target = next_instance_id();
        let token = arm_wait_edges(vec![held], target).expect("fresh ids cannot close a cycle");
        assert_eq!(WAIT_EDGES.lock().get(&held), Some(&target));
        drop(token);
        assert_eq!(
            WAIT_EDGES.lock().get(&held),
            None,
            "edges must clear on release"
        );
    }

    #[test]
    fn fast_path_with_no_held_locks_never_touches_the_graph() {
        let target = next_instance_id();
        let token = arm_wait_edges(Vec::new(), target).expect("empty chain cannot cycle");
        assert_eq!(WAIT_EDGES.lock().get(&target), None);
        drop(token);
        assert_eq!(WAIT_EDGES.lock().get(&target), None);
    }

    #[test]
    fn a_two_instance_wait_cycle_fails_deterministically() {
        let a = next_instance_id();
        let b = next_instance_id();
        let parked = arm_wait_edges(vec![a], b).expect("the first arming cannot cycle");
        let err = arm_wait_edges(vec![b], a).expect_err("the cycle must close here");
        assert_eq!(err.code, "reentrancy_cycle");
        drop(parked);
        assert_eq!(
            WAIT_EDGES.lock().get(&a),
            None,
            "error path cleans its edges"
        );
        assert_eq!(WAIT_EDGES.lock().get(&b), None);
    }

    #[test]
    fn a_three_instance_cycle_names_the_closed_chain() {
        let a = next_instance_id();
        let b = next_instance_id();
        let c = next_instance_id();
        let first = arm_wait_edges(vec![a], b).expect("a waits for b");
        let second = arm_wait_edges(vec![b], c).expect("b waits for c");
        let err = arm_wait_edges(vec![c], a).expect_err("c closing on a deadlocks");
        assert_eq!(err.code, "reentrancy_cycle");
        assert!(
            err.message.contains(&format!("{a} -> {b} -> {c} -> {a}")),
            "chain must name the cycle: {}",
            err.message
        );
        drop(first);
        drop(second);
        for id in [a, b, c] {
            assert_eq!(WAIT_EDGES.lock().get(&id), None);
        }
    }

    #[test]
    fn converging_waits_without_a_cycle_park_normally() {
        let a = next_instance_id();
        let b = next_instance_id();
        let c = next_instance_id();
        let d = next_instance_id();
        let first = arm_wait_edges(vec![a], d).expect("a waits for d");
        let second = arm_wait_edges(vec![b], d).expect("b waits for d too");
        let third = arm_wait_edges(vec![c], a).expect("c waits for a: no cycle, d is free");
        drop(first);
        drop(second);
        drop(third);
        for id in [a, b, c, d] {
            assert_eq!(WAIT_EDGES.lock().get(&id), None);
        }
    }

    #[tokio::test]
    async fn cancelling_a_parking_future_removes_its_edges() {
        let held = next_instance_id();
        let target = next_instance_id();
        let lock: InstanceLock = Arc::new(AsyncMutex::new(()));
        let gate = Arc::clone(&lock).lock_owned().await;
        let task = tokio::spawn({
            let lock = Arc::clone(&lock);
            async move {
                let _outer = ChainGuard::hold(held);
                let _hold = enter_exclusive(&lock, target).await;
            }
        });
        let mut armed = false;
        for _ in 0..200 {
            if !WAIT_EDGES.lock().is_empty() {
                armed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(armed, "the parked entry must have armed its edges");
        task.abort();
        let _ = task.await;
        assert_eq!(
            WAIT_EDGES.lock().get(&held),
            None,
            "cancellation must clean up the armed edges"
        );
        drop(gate);
    }
}
