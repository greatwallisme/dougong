// Rust guideline compliant 2026-10-28
//! Stable C-ABI contract between a [`dougong`] host and dynamically loaded
//! plugins.
//!
//! This crate is the only thing a plugin (`cdylib`) needs to depend on. It
//! deliberately contains no kernel and no async runtime: both sides of the
//! boundary speak raw C-ABI functions and caller-owned buffers only. All
//! framework semantics (effects, provide/withdraw, reactive notification) are
//! applied by the host; the plugin only computes. This "inverted API" avoids
//! duplicated runtime state across compilation artifacts (redoop/cordis-rust
//! validated the pattern; see `doc/research/study-3-rust-projects.md` §3.1).
//!
//! # Outbound calls and grants (ABI v4)
//!
//! Since v3 the boundary is two-way: `setup` receives a [`HostExports`]
//! vtable through which the plugin may call other plugins' services
//! (`require`, synchronous blocking) and publish events to the kernel bus
//! (`emit`, fire-and-forget). The contract points:
//!
//! - The [`HostExports`] pointer passed to `setup` is valid only for the
//!   duration of that call; the plugin copies the struct by value. The copied
//!   entries remain callable until *this activation's* `teardown` returns;
//!   any later call is undefined behavior (not detected). A re-activation
//!   receives a fresh vtable independent of the previous handle.
//! - `require`/`emit`/`grant` may be called from any plugin thread,
//!   concurrently. The host implementations are panic-contained: a host-side
//!   panic becomes [`STATUS_HOST_ERROR`], never an unwind across the boundary.
//!   The plugin must uphold the same rule in the other direction.
//! - `require` honors the capability model: calling a service absent from
//!   the descriptor `inject` list and not covered by an activation grant
//!   deterministically fails with [`STATUS_UNDECLARED`]; an uncommitted
//!   service fails with [`STATUS_INACTIVE`]; a call that would re-enter an
//!   instance already executing on the calling thread fails with
//!   [`STATUS_REENTRANCY`] instead of deadlocking. `in` payloads are uncapped;
//!   `out` payloads follow the growable buffer protocol (retry on
//!   [`STATUS_CAPACITY`]).
//! - `grant` lets a provider authorize one committed target activation to
//!   `require` one of its descriptor-provided services without static
//!   injection. It is an idempotent, status-only registration primitive: no
//!   foreign call, no output buffer, no instance lock, and no reentrancy
//!   participation. Grants expire with the granter activation and are never
//!   inherited by a later activation.
//! - On any `require` status other than [`STATUS_OK`]/[`STATUS_CAPACITY`]
//!   the out buffer may carry a UTF-8 JSON detail document (`{"code": ...,
//!   "class": ..., "message": ...}` — `class` is the kernel's error class,
//!   one of `"logic"` / `"timeout"` / `"transport"`, added additively in
//!   v0.3.0's stability track); under [`STATUS_FAILED`] it conveys
//!   the target service's own failure. As with `call`, the host may
//!   re-execute the invocation when the caller retries with a larger buffer.
//! - `emit` payloads must be valid JSON bytes ([`STATUS_INVALID_PAYLOAD`]
//!   otherwise); event names must be UTF-8 and must not use the `internal/*`
//!   prefix ([`STATUS_INVALID_EVENT`]). Emission is enqueue-and-return: no
//!   ordering or delivery result is reported to the caller.
//! - `realm_spawn` / `realm_retire` / `realm_require` (ABI v6, D33) let an
//!   activation place child plugin activations in a realm it owns and call the
//!   services those children provide. The children's fibers are effects of the
//!   spawning activation: its teardown cascades to them. All three are
//!   capability-narrow: a realm label or child handle is valid only for the
//!   activation that received it, and `realm_require` reaches only services
//!   provided by that activation's own children. `realm_spawn` and
//!   `realm_retire` must originate on a plugin-owned thread (a foreign entry
//!   frame on the calling thread fails with [`STATUS_REENTRANCY`]).
//! - Thread discipline: the plugin must terminate and join its own threads
//!   before this activation's `teardown` returns (the per-instance lock does
//!   not cover plugin-owned threads). This is contractual, not enforced.
//!
//! # Portability
//!
//! Every type in this crate is `#[repr(C)]` and interacts with no statics, no
//! `TypeId`, and no non-portable data. Buffers are caller-owned: the plugin
//! never allocates anything the host must free, and the host never frees
//! plugin allocations (plugin-private state is created and released by the
//! plugin's own `setup`/`teardown` pair).
//!
//! # Unloading
//!
//! A host must not unmap a plugin library while any of its handles, function
//! pointers, or in-flight disposers may still be invoked. dougong hosts keep
//! every plugin mapped for the lifetime of the process and release only
//! plugin state through `teardown` (design decision D5, user ruling R1).
//!
//! # Example
//!
//! A minimal plugin export table:
//!
//! ```
//! use dougong_abi::{
//!     write_out, ABI_VERSION, HostExports, MODE_EMIT, PluginExports, STATUS_FAILED, STATUS_OK,
//! };
//!
//! extern "C" fn name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
//!     // SAFETY: the host provides a valid `cap`-sized buffer and a writable
//!     // `len` slot, as required by the export-table contract.
//!     if unsafe { write_out(b"greeter", out, cap, len) } { 0 } else { -1 }
//! }
//!
//! extern "C" fn descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
//!     // SAFETY: same host buffer contract as `name`.
//!     if unsafe { write_out(br#"{"provide":[]}"#, out, cap, len) } { 0 } else { -1 }
//! }
//!
//! unsafe extern "C" fn setup(
//!     _host: *const HostExports,
//!     _config: *const u8,
//!     _config_len: usize,
//! ) -> *mut std::os::raw::c_void {
//!     // Returning null signals initialization failure. The host exports
//!     // pointer and the activation config bytes are valid for this call
//!     // only; copy the vtable to use `require`/`emit` from the plugin's own
//!     // threads later.
//!     std::ptr::null_mut()
//! }
//!
//! unsafe extern "C" fn call(
//!     _handle: *mut std::os::raw::c_void,
//!     _method: u32,
//!     _data: *const u8,
//!     _len: usize,
//!     _out: *mut u8,
//!     _cap: usize,
//!     _out_len: *mut usize,
//! ) -> i32 {
//!     -2
//! }
//!
//! unsafe extern "C" fn on_event(
//!     _handle: *mut std::os::raw::c_void,
//!     _event: u32,
//!     _mode: u32,
//!     _data: *const u8,
//!     _len: usize,
//!     _next_available: u32,
//!     _out: *mut u8,
//!     _cap: usize,
//!     _out_len: *mut usize,
//! ) -> i32 {
//!     -2
//! }
//!
//! unsafe extern "C" fn on_next(
//!     _handle: *mut std::os::raw::c_void,
//!     _event: u32,
//!     chain_status: i32,
//!     _data: *const u8,
//!     _len: usize,
//!     _out: *mut u8,
//!     _cap: usize,
//!     _out_len: *mut usize,
//! ) -> i32 {
//!     if chain_status == STATUS_OK {
//!         STATUS_OK
//!     } else {
//!         STATUS_FAILED
//!     }
//! }
//!
//! unsafe extern "C" fn teardown(_handle: *mut std::os::raw::c_void) {}
//!
//! #[unsafe(no_mangle)]
//! static DOUGONG_PLUGIN_EXPORTS: PluginExports = PluginExports {
//!     abi_version: ABI_VERSION,
//!     name,
//!     descriptor,
//!     setup,
//!     call,
//!     on_event,
//!     on_next,
//!     teardown,
//! };
//!
//! # fn check() {
//! assert_eq!(DOUGONG_PLUGIN_EXPORTS.abi_version, ABI_VERSION);
//! assert_eq!(MODE_EMIT, 0);
//! # }
//! ```
//!
//! [`dougong`]: ../dougong/index.html

// The SAFETY notes below rely on one shared precondition, so state it once
// here: every function pointer in [`PluginExports`] is called by the host with
// (1) `out_buf` valid for `cap` writable bytes, (2) `out_len` pointing at one
// writable `usize`, and (3) `in_data` valid for `in_len` readable bytes.
// Handles are opaque values previously returned by `setup` and not yet passed
// to `teardown`.

/// Status code: the callee completed and produced output.
pub const STATUS_OK: i32 = 0;

/// Status code for event dispatch: the callee bailed. Output carries the bail
/// value and the dispatch chain stops (mirrors cordis `isBailed`).
pub const STATUS_BAILED: i32 = 1;

/// Status code for waterfall event dispatch: the callee wants the rest of
/// the chain.
///
/// The host resolves the continuation asynchronously and delivers its result
/// through the `on_next` export; the callee's post-processed reply becomes
/// the chain value at this listener.
pub const STATUS_DEFERRED: i32 = 2;

/// Status code: the provided output buffer is too small for the payload.
pub const STATUS_CAPACITY: i32 = -1;

/// Status code: the callee failed for a plugin-internal reason. The host logs
/// and treats the operation as unsuccessful; no unwinding obligation exists
/// beyond the effect/disposer contract.
///
/// Since v3, a `call` callee may additionally leave a best-effort UTF-8
/// diagnostic in the out buffer (failure is final; this path never retries).
pub const STATUS_FAILED: i32 = -2;

/// Status code for outbound `require`: the service was never declared in the
/// plugin's descriptor `inject` list (capability error, mirrors the kernel's
/// `undeclared_dependency`).
pub const STATUS_UNDECLARED: i32 = -3;

/// Status code for outbound `require`: the service is declared but no
/// provider is currently committed (mirrors `inactive_dependency`).
pub const STATUS_INACTIVE: i32 = -4;

/// Status code for outbound `require`: same-thread reentrancy cycle.
///
/// The call would re-enter an instance already executing on the calling
/// thread; the deterministic rejection replaces what would otherwise be a
/// self-deadlock on the per-instance lock.
pub const STATUS_REENTRANCY: i32 = -5;

/// Status code: the host failed internally while serving a vtable call,
/// including a caught host-side panic.
pub const STATUS_HOST_ERROR: i32 = -6;

/// Status code for outbound `emit`: the payload was not valid JSON bytes.
pub const STATUS_INVALID_PAYLOAD: i32 = -7;

/// Status code for outbound `emit`: the event name was rejected (not UTF-8,
/// or reserved under the `internal/*` prefix).
pub const STATUS_INVALID_EVENT: i32 = -8;

/// ABI revision of this contract. Hosts reject plugins whose
/// [`PluginExports::abi_version`] differs from their own value.
///
/// v2 (2026-10-21): event dispatch became CPS-capable — `on_event` reports
/// `STATUS_DEFERRED` to pull the waterfall continuation, the host delivers it
/// through the new `on_next` export, and the unusable synchronous `NextFn`
/// was removed (design decision D14).
///
/// v3 (2026-10-23): outbound capability — `setup` now receives a
/// [`HostExports`] vtable carrying `require` (synchronous cross-plugin
/// service calls) and `emit` (kernel-bus event publication).
///
/// v4 (2026-10-28): callback capability grants — [`HostExports`] gains
/// `grant`, an activation-scoped registration primitive for narrow runtime
/// delegation of `require` capability (design decision D31).
///
/// v5 (2026-11-02): outbound synchronous dispatch — [`HostExports`] gains
/// `dispatch`, one synchronous kernel-bus event publication per call whose
/// await-style modes (`MODE_PARALLEL`/`MODE_SERIAL`/`MODE_BAIL`/
/// `MODE_WATERFALL`) block the calling thread until the chain settles and
/// write the chain value into `out_buf` (`MODE_EMIT` enqueues and returns,
/// mirroring the `emit` entry). Waterfall composes around a fixed identity
/// continuation: an untouched chain returns the original payload bytes
/// (design decision D32; `dispatch_requirements.md` §2).
///
/// v6 (2026-11-05): outbound realm activation — [`HostExports`] gains
/// `realm_spawn`, `realm_retire`, and `realm_require`: an activation opens a
/// realm, activates child plugin artifacts inside it (children are effects of
/// the spawning activation's fiber), retires them individually, and calls the
/// services they provide (design decision D33; `realm_requirements.md` §2).
///
/// v7 (2026-11-07): activation config delivery — `setup` now receives the
/// activation's config as JSON bytes (`config`/`config_len`), matching what
/// in-process plugins already get through `Plugin::apply` and wasm guests
/// through their `apply` payload. Earlier revisions dropped the config at
/// the dylib boundary, forcing per-plugin config baking (design decision
/// D34; `config_requirements.md` §2).
///
/// v8 (2026-11-14): cross-activation registration ownership —
/// [`HostExports`] gains `self_activation` / `caller_activation` / `watch` /
/// `activation_alive`: a plugin can name itself and its inbound callers, and
/// register a kernel-driven teardown delivery so registrations it made into
/// another plugin's service are reclaimed with its activation
/// (`ownership_requirements.md`; the boundary form's effect face).
pub const ABI_VERSION: u32 = 8;

/// Minimum `out_buf` capacity `realm_spawn` accepts.
///
/// The success reply (`{"realm":L,"child":C,"state":"active"|"pending"}`)
/// always fits within it, and a smaller buffer is rejected with
/// [`STATUS_CAPACITY`] *before* any side effect, so a retry with a larger
/// buffer never spawns twice.
pub const REALM_SPAWN_REPLY_MIN: usize = 96;

/// Event dispatch mode: run listeners without waiting for returned values.
pub const MODE_EMIT: u32 = 0;
/// Event dispatch mode: run listeners concurrently and wait for all.
pub const MODE_PARALLEL: u32 = 1;
/// Event dispatch mode: run listeners in order until one bails.
pub const MODE_SERIAL: u32 = 2;
/// Event dispatch mode: run listeners without awaiting until one bails.
pub const MODE_BAIL: u32 = 3;
/// Event dispatch mode: compose listeners around a continuation callback.
pub const MODE_WATERFALL: u32 = 4;

/// Copies `src` into a caller-owned output buffer and stores the written
/// length.
///
/// This is the canonical implementation of the out-buffer protocol used by
/// every callback in [`PluginExports`]. Plugin authors should call it rather
/// than re-implementing pointer arithmetic.
///
/// # Safety
///
/// The caller must guarantee that `out_buf` is valid for `cap` writable bytes
/// and that `out_len` points to one writable `usize`. Both are contractual
/// for host-provided buffers as documented at [`PluginExports`]. Violating
/// either requirement is undefined behavior.
#[must_use]
pub unsafe fn write_out(src: &[u8], out_buf: *mut u8, cap: usize, out_len: *mut usize) -> bool {
    if src.len() > cap {
        return false;
    }
    // SAFETY: the guard above establishes `cap >= src.len()`; buffer validity
    // is the caller contract documented in `# Safety`. The copy is
    // non-overlapping because source and destination live on opposite sides
    // of the ABI boundary. The `let () =` binding places the block in
    // expression position, which `semicolon_outside_block` and
    // `semicolon_if_nothing_returned` both require.
    let () = unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), out_buf, src.len());
        *out_len = src.len();
    };
    true
}

/// Host-provided outbound vtable handed to `setup` (ABI v8).
///
/// The pointer passed to `setup` is valid for the duration of that call
/// only; the plugin copies this struct by value. `host_handle` is an opaque
/// activation token the plugin passes back verbatim on every call — it binds
/// each call to the calling activation's capability set and isolation realm.
/// The copied entries remain callable from any thread until this
/// activation's `teardown` returns; any later use is undefined behavior
/// (documented, not detected).
///
/// # Contract
///
/// - `require` performs one synchronous, blocking cross-plugin service call
///   through the dynamic channel (`service` name + `method` selector + byte
///   payload). It returns [`STATUS_OK`] (payload in `out_buf`),
///   [`STATUS_CAPACITY`] (retry with a larger buffer), [`STATUS_UNDECLARED`],
///   [`STATUS_INACTIVE`], [`STATUS_REENTRANCY`], [`STATUS_FAILED`], or
///   [`STATUS_HOST_ERROR`]; failure statuses may carry a JSON detail document
///   in `out_buf`. Input payloads are uncapped; output follows the growable
///   buffer protocol.
/// - `emit` publishes one event to the kernel bus, enqueue-and-return. The
///   event name must be UTF-8 outside the `internal/*` prefix and the
///   payload must be valid JSON bytes.
/// - `grant` allows the currently committed provider of `target_service` to
///   `require` this plugin's `granted_service` without a static descriptor
///   injection. The granted service must occur in this plugin's `provide`
///   table. The grant binds the resolved target activation and expires with
///   the granting activation. It returns [`STATUS_OK`], including for an
///   idempotent repeated grant, [`STATUS_INACTIVE`] when the target has no
///   committed provider, or [`STATUS_FAILED`] for an invalid name or a
///   service the granter does not own. No output buffer is written.
/// - `dispatch` runs one synchronous event dispatch in any of the five modes
///   (ABI v5); see the field documentation.
/// - `realm_spawn` activates a child plugin artifact in a realm owned by this
///   activation (ABI v6). Realm `0` allocates a fresh realm; a non-zero
///   realm must be one an earlier spawn returned. The call blocks until the
///   child settles: success replies with a JSON document carrying the realm
///   label, a child handle, and the settled state (`"active"`, or `"pending"`
///   when the child waits on a dependency that is not yet committed). A
///   child that fails to start is disposed and reported as
///   [`STATUS_FAILED`] with the usual JSON detail.
/// - `realm_retire` disposes one child this activation spawned and returns
///   once the child's teardown has settled; it writes no output buffer.
/// - `realm_require` is `require` addressed to a realm: it resolves the
///   service in the named realm and authorizes the call only when the
///   provider is a child this activation spawned (no `inject` declaration is
///   needed or consulted).
/// - `self_activation` answers this activation's process-unique id and
///   `caller_activation` the caller activation of the inbound `call` frame
///   being dispatched (ABI v8); the latter fails with `no_caller_activation`
///   outside such a frame.
/// - `watch` registers a kernel-driven teardown delivery to another service
///   (ABI v8): when this activation is disposed the kernel calls the bound
///   method with `{"owner": <this id>}`. Delivery survives registration
///   failures and never fails the disposal; records die with the activation.
/// - `activation_alive` is the read-only liveness probe (ABI v8): the
///   backstop a callee uses to prune registrations whose owner's teardown was
///   orphaned by the cleanup budget.
///
/// All callable entries are implemented by the host with full panic
/// containment and are safe to call concurrently from arbitrary plugin
/// threads. `grant` and `watch` only register: they neither invoke a plugin
/// nor participate in reentrancy detection.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostExports {
    /// Opaque per-activation token; passed back verbatim on every call.
    pub host_handle: *mut std::os::raw::c_void,
    /// Synchronously calls `service`'s `method` with the `in_data` payload.
    pub require: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        service: *const u8,
        service_len: usize,
        method: u32,
        in_data: *const u8,
        in_len: usize,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Publishes `event` with a JSON `payload` to the kernel bus.
    pub emit: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        event: *const u8,
        event_len: usize,
        payload: *const u8,
        payload_len: usize,
    ) -> i32,
    /// Grants `target_service`'s committed activation access to `granted_service`.
    ///
    /// This is a pure registration primitive: it initiates no calls and writes
    /// no output buffer. Both names are read-only byte strings for this call.
    pub grant: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        target_service: *const u8,
        target_service_len: usize,
        granted_service: *const u8,
        granted_service_len: usize,
    ) -> i32,
    /// Publishes `event` with a JSON `payload` to the kernel bus, dispatched
    /// in `mode` (one of the [`MODE_*`](MODE_EMIT) constants).
    ///
    /// Await-style modes block the calling thread until the dispatch settles
    /// and follow the same growable `out_buf` protocol as `require`:
    /// `MODE_WATERFALL` composes the listener chain around a fixed identity
    /// continuation (an untouched chain reports the original payload bytes),
    /// `MODE_SERIAL`/`MODE_BAIL` report the bail value, and `MODE_PARALLEL`
    /// reports the settled state. A listener failure is `STATUS_FAILED` with
    /// a JSON `{"code","class","message"}` detail in `out_buf` — dispatch is
    /// request semantics, never the fire-and-forget broadcast of `emit`.
    /// `MODE_EMIT` enqueues and returns without writing `out_buf`.
    ///
    /// The call must originate on a plugin-owned thread: invoking it while
    /// any foreign entry executes on the calling thread fails deterministically
    /// with [`STATUS_REENTRANCY`] (the chain may legally `require` the
    /// dispatcher's own services, and a chain-side wait on that instance lock
    /// is invisible to the wait-cycle detector). `internal/*` event names are
    /// rejected like the `emit` entry.
    pub dispatch: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        event: *const u8,
        event_len: usize,
        mode: u32,
        payload: *const u8,
        payload_len: usize,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Activates the dylib artifact at `path` (UTF-8) as a child of this
    /// activation inside `realm`, blocking until the child settles.
    ///
    /// `realm == 0` allocates a fresh realm; any other value must be a realm
    /// label an earlier `realm_spawn` of this activation returned
    /// (`unknown_realm` otherwise). `out_buf` must hold at least
    /// [`REALM_SPAWN_REPLY_MIN`] bytes; a smaller buffer is answered with
    /// [`STATUS_CAPACITY`] before anything happens. On [`STATUS_OK`] the
    /// buffer carries `{"realm":L,"child":C,"state":"active"|"pending"}`.
    /// Failures are [`STATUS_FAILED`] with a JSON `{"code","class","message"}`
    /// detail (`realm_spawn_load_failed` for an unloadable artifact, the
    /// child's own startup error otherwise) and leave no residue.
    ///
    /// The call must originate on a plugin-owned thread: a foreign entry
    /// frame on the calling thread fails with [`STATUS_REENTRANCY`] (the child
    /// may require this activation's own services during its setup, which a
    /// held instance lock would turn into an undetectable wait).
    pub realm_spawn: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        realm: u64,
        path: *const u8,
        path_len: usize,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Disposes the child `child` (a handle `realm_spawn` returned for this
    /// activation) and returns once its teardown has settled.
    ///
    /// Status-only like `grant`: [`STATUS_OK`], [`STATUS_FAILED`] for an
    /// unknown or already retired handle, or [`STATUS_REENTRANCY`] when a
    /// foreign entry frame is on the calling thread.
    pub realm_retire: unsafe extern "C" fn(host: *mut std::os::raw::c_void, child: u64) -> i32,
    /// `require` addressed to `realm`: resolves `service` in that realm and
    /// calls `method` with the `in_data` payload.
    ///
    /// The realm must be one this activation opened (`unknown_realm`
    /// otherwise) and the resolved provider must be a child this activation
    /// spawned ([`STATUS_UNDECLARED`] otherwise); no descriptor `inject`
    /// declaration is consulted. Statuses, detail documents, reentrancy
    /// behavior, and the growable `out_buf` protocol are those of `require`.
    pub realm_require: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        realm: u64,
        service: *const u8,
        service_len: usize,
        method: u32,
        in_data: *const u8,
        in_len: usize,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Answers this activation's process-unique id as `{"activation": N}`.
    ///
    /// Valid on every thread of the plugin: `setup`, plugin-owned threads,
    /// and inside any inbound frame. The id is minted once per activation and
    /// never reused (`ownership_requirements.md` §3.2).
    pub self_activation: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Answers the caller activation's id of the inbound service call this
    /// thread is dispatching, as `{"activation": N}`.
    ///
    /// Only a request-semantics `call` frame carries a caller: the value is
    /// unset in `setup`, the teardown export, event-bridge frames, and on
    /// plugin-owned threads, where the entry fails with [`STATUS_FAILED`] and
    /// the `no_caller_activation` detail document.
    pub caller_activation: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Registers one teardown delivery: when THIS activation is disposed the
    /// kernel calls `method` on `service` with `{"owner": <this id>}`.
    ///
    /// Pure registration primitive (status-only like `grant`): it initiates
    /// no call and writes no output buffer. The target resolves in the
    /// caller's realm at registration and is bound to the provider activation
    /// it resolved to; delivery is kernel-initiated (the caller's `inject`
    /// and grant tables are not consulted). Records die with the activation
    /// even when the delivery fails.
    pub watch: unsafe extern "C" fn(
        host: *mut std::os::raw::c_void,
        service: *const u8,
        service_len: usize,
        method: u32,
    ) -> i32,
    /// Read-only liveness probe: [`STATUS_OK`] while `activation` is live,
    /// [`STATUS_INACTIVE`] for an id that was never issued or has been
    /// reclaimed.
    ///
    /// The backstop for callees that keep registration tables keyed by owner
    /// activation id: an activation whose teardown was orphaned by the
    /// cleanup budget is already "not live" (its liveness gate flipped),
    /// so the callee can prune the residue without waiting for the `watch`
    /// delivery.
    pub activation_alive:
        unsafe extern "C" fn(host: *mut std::os::raw::c_void, activation: u64) -> i32,
}

impl std::fmt::Debug for HostExports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Function pointers and the opaque token carry no debuggable state.
        f.debug_struct("HostExports").finish_non_exhaustive()
    }
}

/// The complete export table a plugin library must expose as
/// `#[unsafe(no_mangle)] pub static DOUGONG_PLUGIN_EXPORTS: PluginExports`.
///
/// Hosts resolve this symbol once at load time and reject the plugin when
/// `abi_version` mismatches. All buffers follow the caller-owned protocol
/// documented at the crate level.
///
/// # Contract
///
/// - `setup` receives the [`HostExports`] vtable (valid for the call's
///   duration; copy it to keep it), the activation config as JSON bytes
///   (`config`/`config_len` — the verbatim serialization of the config
///   `Value`, so an activation without config carries the JSON text `null`,
///   exactly as wasm guests receive it; a null pointer accompanies a zero
///   length as a defensive ABI signal), and returns an opaque
///   plugin-private handle, or null to signal initialization failure.
///   The host calls it exactly once per fiber activation, under the
///   per-instance lock.
/// - `call` serves one invocation of one provided service method. Method
///   identifiers are constants of the service contract (WIT-derived), not
///   global values.
/// - `on_event` receives one dispatched event. `mode` is one of the
///   [`MODE_*`](MODE_EMIT) constants; `next_available` is `1` only for
///   [`MODE_WATERFALL`]. Returning [`STATUS_DEFERRED`] in waterfall pulls the
///   rest of the chain: any output written before deferring is the payload
///   the chain continues with (empty output continues with the original), the
///   host later calls `on_next` with the continuation result, and this
///   export's post-processed output becomes the chain value here. Any other
///   status vetoes the chain with the produced output.
/// - `on_next` delivers the deferred waterfall continuation. `chain_status`
///   is [`STATUS_OK`] when `data`/`len` carry the continuation reply and
///   [`STATUS_FAILED`] when the chain errored. Returning [`STATUS_DEFERRED`]
///   is a contract violation; the host treats it as [`STATUS_FAILED`].
/// - `teardown` releases the handle returned by `setup`. The host calls it
///   exactly once per successful `setup`, after all disposers referencing the
///   handle have been drained.
///
/// The host serializes every foreign call that shares one `handle` through a
/// per-instance lock, so plugin-private state needs no internal locking even
/// when several consumer fibers call concurrently.
#[repr(C)]
pub struct PluginExports {
    /// ABI revision; must equal [`ABI_VERSION`] at load time.
    pub abi_version: u32,
    /// Writes the plugin display name (UTF-8) into the output buffer.
    pub name: unsafe extern "C" fn(out_buf: *mut u8, cap: usize, out_len: *mut usize) -> i32,
    /// Writes the plugin descriptor (UTF-8 JSON: name, version, provide,
    /// inject, listen) into the output buffer.
    pub descriptor: unsafe extern "C" fn(out_buf: *mut u8, cap: usize, out_len: *mut usize) -> i32,
    /// Allocates plugin-private state; null signals initialization failure.
    /// The host exports pointer is valid for this call only. `config` points
    /// at the activation config's verbatim JSON serialization for this call
    /// only (`null` text when the activation carries none, as for wasm
    /// guests).
    pub setup: unsafe extern "C" fn(
        host: *const HostExports,
        config: *const u8,
        config_len: usize,
    ) -> *mut std::os::raw::c_void,
    /// Serves one provided-service method invocation.
    pub call: unsafe extern "C" fn(
        handle: *mut std::os::raw::c_void,
        method: u32,
        in_data: *const u8,
        in_len: usize,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Receives one dispatched event in the given mode.
    pub on_event: unsafe extern "C" fn(
        handle: *mut std::os::raw::c_void,
        event: u32,
        mode: u32,
        in_data: *const u8,
        in_len: usize,
        next_available: u32,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Delivers the deferred waterfall continuation requested through
    /// [`STATUS_DEFERRED`] (design decision D14).
    pub on_next: unsafe extern "C" fn(
        handle: *mut std::os::raw::c_void,
        event: u32,
        chain_status: i32,
        data: *const u8,
        len: usize,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,
    /// Releases plugin-private state; called exactly once per live handle.
    pub teardown: unsafe extern "C" fn(handle: *mut std::ffi::c_void),
}

impl std::fmt::Debug for PluginExports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Function pointers carry no debuggable state; only the version and
        // identity are meaningful for diagnostics.
        f.debug_struct("PluginExports")
            .field("abi_version", &self.abi_version)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_out_copies_and_reports_length() {
        let mut buf = [0_u8; 8];
        let mut len: usize = 0;
        // SAFETY: `buf` is a live stack array of `buf.len()` bytes and `len`
        // is a live stack `usize`.
        let ok = unsafe { write_out(b"abc", buf.as_mut_ptr(), buf.len(), &raw mut len) };
        assert!(ok);
        assert_eq!(len, 3);
        assert_eq!(&buf[..3], b"abc");
    }

    #[test]
    fn write_out_rejects_oversized_source() {
        let mut buf = [0_u8; 2];
        let mut len: usize = 0;
        // SAFETY: same live stack-buffer contract as above.
        let ok = unsafe { write_out(b"abc", buf.as_mut_ptr(), buf.len(), &raw mut len) };
        assert!(!ok);
        assert_eq!(len, 0);
    }

    #[test]
    fn write_out_accepts_exact_capacity() {
        let mut buf = [0_u8; 3];
        let mut len: usize = 0;
        // SAFETY: same live stack-buffer contract as above.
        let ok = unsafe { write_out(b"abc", buf.as_mut_ptr(), buf.len(), &raw mut len) };
        assert!(ok);
        assert_eq!(len, 3);
    }

    #[test]
    fn event_modes_match_contract_order() {
        assert_eq!(MODE_EMIT, 0);
        assert_eq!(MODE_PARALLEL, 1);
        assert_eq!(MODE_SERIAL, 2);
        assert_eq!(MODE_BAIL, 3);
        assert_eq!(MODE_WATERFALL, 4);
    }

    #[test]
    fn abi_version_is_stable() {
        assert_eq!(ABI_VERSION, 8);
        assert_eq!(STATUS_DEFERRED, 2);
    }

    #[test]
    fn realm_spawn_reply_fits_its_minimum_capacity() {
        let widest = format!(
            r#"{{"realm":{},"child":{},"state":"pending"}}"#,
            u64::MAX,
            u64::MAX
        );
        assert!(widest.len() <= REALM_SPAWN_REPLY_MIN);
    }

    #[test]
    fn outbound_status_codes_are_stable() {
        assert_eq!(STATUS_UNDECLARED, -3);
        assert_eq!(STATUS_INACTIVE, -4);
        assert_eq!(STATUS_REENTRANCY, -5);
        assert_eq!(STATUS_HOST_ERROR, -6);
        assert_eq!(STATUS_INVALID_PAYLOAD, -7);
        assert_eq!(STATUS_INVALID_EVENT, -8);
    }
}
