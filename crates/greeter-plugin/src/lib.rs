// Rust guideline compliant 2026-10-21
//! Example dougong plugin as a pure C-ABI cdylib.
//!
//! Depends only on `dougong-abi`: no kernel, no async runtime crosses the
//! boundary. All framework semantics run host-side; this library exposes an
//! export table, one provided service (`greeting`) invocable through the
//! dynamic channel with method [`GREETER_HELLO`], and two listened events:
//!
//! - `ping` (event id [`GREETER_EV_PING`]): replies with a quoted JSON pong.
//! - `upper` (event id [`GREETER_EV_UPPER`]): pulls the waterfall
//!   continuation by returning `STATUS_DEFERRED` and upper-cases the chain
//!   reply in `on_next` (ABI v2 CPS protocol, design decision D14).

use std::io::Write;

use dougong_abi::{ABI_VERSION, PluginExports, write_out};

/// Method selector: returns the greeting bytes.
pub const GREETER_HELLO: u32 = 0;

/// Event id: `ping` — echo a pong.
pub const GREETER_EV_PING: u32 = 1;

/// Event id: `upper` — waterfall pull, upper-case the chain reply.
pub const GREETER_EV_UPPER: u32 = 2;

const GREETING: &[u8] = b"hello from a dougong dylib";
const PONG: &[u8] = b"\"pong from greeter-dylib\"";

struct State {
    invocations: u32,
    /// Set between a `STATUS_DEFERRED` waterfall answer and the matching
    /// `on_next` call. The host serializes every foreign call per instance,
    /// so no other event can interleave here.
    pending_upper: bool,
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
    let descriptor = br#"{"name":"greeter","version":"0.2.0","provide":["greeting"],"inject":[],"listen":{"ping":1,"upper":2}}"#;
    // SAFETY: same host buffer contract as `name`.
    if unsafe { write_out(descriptor, out, cap, len) } {
        dougong_abi::STATUS_OK
    } else {
        dougong_abi::STATUS_CAPACITY
    }
}

unsafe extern "C" fn setup() -> *mut std::os::raw::c_void {
    let state = Box::new(State {
        invocations: 0,
        pending_upper: false,
    });
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
    handle: *mut std::os::raw::c_void,
    event: u32,
    _mode: u32,
    _data: *const u8,
    _len: usize,
    next_available: u32,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if handle.is_null() {
        return dougong_abi::STATUS_FAILED;
    }
    // SAFETY: live handle per the export-table contract.
    let state = unsafe { &mut *(handle.cast::<State>()) };
    match event {
        GREETER_EV_PING => {
            // SAFETY: host buffer contract.
            if unsafe { write_out(PONG, out, cap, out_len) } {
                dougong_abi::STATUS_OK
            } else {
                dougong_abi::STATUS_CAPACITY
            }
        }
        GREETER_EV_UPPER => {
            if next_available != 1 {
                // `upper` is a waterfall listener; without a continuation to
                // pull there is nothing to post-process.
                return dougong_abi::STATUS_FAILED;
            }
            state.pending_upper = true;
            dougong_abi::STATUS_DEFERRED
        }
        _ => dougong_abi::STATUS_FAILED,
    }
}

unsafe extern "C" fn on_next(
    handle: *mut std::os::raw::c_void,
    event: u32,
    chain_status: i32,
    data: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if handle.is_null() || event != GREETER_EV_UPPER {
        return dougong_abi::STATUS_FAILED;
    }
    // SAFETY: live handle; the host delivered the continuation this plugin
    // deferred (per-instance calls are serialized by the host).
    let state = unsafe { &mut *(handle.cast::<State>()) };
    if !state.pending_upper {
        return dougong_abi::STATUS_FAILED;
    }
    state.pending_upper = false;
    if chain_status != dougong_abi::STATUS_OK {
        return dougong_abi::STATUS_FAILED;
    }
    // SAFETY: `data` is valid for `len` readable bytes per the host contract.
    let reply = unsafe { std::slice::from_raw_parts(data, len) };
    let upper = reply.to_ascii_uppercase();
    // SAFETY: host buffer contract.
    if unsafe { write_out(&upper, out, cap, out_len) } {
        dougong_abi::STATUS_OK
    } else {
        dougong_abi::STATUS_CAPACITY
    }
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
    on_next,
    teardown,
};
