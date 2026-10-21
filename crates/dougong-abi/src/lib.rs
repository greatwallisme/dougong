// Rust guideline compliant 2026-10-20
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
//!     write_out, PluginExports, ABI_VERSION, MODE_EMIT, STATUS_FAILED, STATUS_OK,
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
//! unsafe extern "C" fn setup() -> *mut std::os::raw::c_void {
//!     // Returning null signals initialization failure.
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
pub const STATUS_FAILED: i32 = -2;

/// ABI revision of this contract. Hosts reject plugins whose
/// [`PluginExports::abi_version`] differs from their own value.
///
/// v2 (2026-10-21): event dispatch became CPS-capable — `on_event` reports
/// `STATUS_DEFERRED` to pull the waterfall continuation, the host delivers it
/// through the new `on_next` export, and the unusable synchronous `NextFn`
/// was removed (design decision D14).
pub const ABI_VERSION: u32 = 2;

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

/// The complete export table a plugin library must expose as
/// `#[unsafe(no_mangle)] pub static DOUGONG_PLUGIN_EXPORTS: PluginExports`.
///
/// Hosts resolve this symbol once at load time and reject the plugin when
/// `abi_version` mismatches. All buffers follow the caller-owned protocol
/// documented at the crate level.
///
/// # Contract
///
/// - `setup` returns an opaque plugin-private handle, or null to signal
///   initialization failure. The host calls it exactly once per fiber.
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
    pub setup: unsafe extern "C" fn() -> *mut std::os::raw::c_void,
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
        assert_eq!(ABI_VERSION, 2);
        assert_eq!(STATUS_DEFERRED, 2);
    }
}
