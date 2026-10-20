// Rust guideline compliant 2026-10-20
//! Example dougong plugin as a pure C-ABI cdylib.
//!
//! Depends only on `dougong-abi`: no kernel, no async runtime crosses the
//! boundary. All framework semantics run host-side; this library exposes an
//! export table plus one provided service (`greeting`) invocable through the
//! dynamic channel with method [`GREETER_HELLO`].

use std::io::Write;

use dougong_abi::{ABI_VERSION, PluginExports, write_out};

/// Method selector: returns the greeting bytes.
pub const GREETER_HELLO: u32 = 0;

const GREETING: &[u8] = b"hello from a dougong dylib";

struct State {
    invocations: u32,
}

unsafe extern "C" fn name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: the host guarantees a `cap`-sized out-buffer and a writable
    // `len` slot (export-table contract).
    if unsafe { write_out(b"greeter", out, cap, len) } {
        dougong_abi::STATUS_OK
    } else {
        dougong_abi::STATUS_CAPACITY
    }
}

unsafe extern "C" fn descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    let descriptor = br#"{"name":"greeter","version":"0.1.0","provide":["greeting"],"inject":[]}"#;
    // SAFETY: same host buffer contract as `name`.
    if unsafe { write_out(descriptor, out, cap, len) } {
        dougong_abi::STATUS_OK
    } else {
        dougong_abi::STATUS_CAPACITY
    }
}

unsafe extern "C" fn setup() -> *mut std::os::raw::c_void {
    let state = Box::new(State { invocations: 0 });
    Box::into_raw(state).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn call(
    handle: *mut std::os::raw::c_void,
    method: u32,
    _data: *const u8,
    _len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if handle.is_null() {
        return dougong_abi::STATUS_FAILED;
    }
    // SAFETY: the host passes back a handle previously returned by `setup`
    // and not yet released via `teardown`.
    let state = unsafe { &mut *(handle.cast::<State>()) };
    if method != GREETER_HELLO {
        return dougong_abi::STATUS_FAILED;
    }
    state.invocations += 1;
    // SAFETY: same host buffer contract as `name`.
    if unsafe { write_out(GREETING, out, cap, out_len) } {
        dougong_abi::STATUS_OK
    } else {
        dougong_abi::STATUS_CAPACITY
    }
}

unsafe extern "C" fn on_event(
    _handle: *mut std::os::raw::c_void,
    _event: u32,
    _mode: u32,
    _data: *const u8,
    _len: usize,
    _next: Option<dougong_abi::NextFn>,
    _next_ctx: *mut usize,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    dougong_abi::STATUS_OK
}

unsafe extern "C" fn teardown(handle: *mut std::os::raw::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the handle was produced by `setup` and torn down exactly once.
    let state = unsafe { Box::from_raw(handle.cast::<State>()) };
    // Best-effort sink: stderr write failures are ignored because teardown
    // logging must never take the process down.
    let _ = writeln!(
        std::io::stderr(),
        "[greeter(dylib)] teardown after {} invocation(s)",
        state.invocations
    );
}

/// Export table for the greeter demo plugin.
///
/// The host resolves this symbol by name at load time.
#[unsafe(no_mangle)]
pub static DOUGONG_PLUGIN_EXPORTS: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name,
    descriptor,
    setup,
    call,
    on_event,
    teardown,
};
