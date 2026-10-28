// Rust guideline compliant 2026-10-28
//! Example grant-target dougong plugin as a pure C-ABI cdylib.
//!
//! Depends only on `dougong-abi`. The descriptor `inject` list is empty on
//! purpose: this plugin is the authorized caller of the `outbound_demo`
//! grant arm (E2), where the greeter plugin grants its activation the
//! `greeting` service at runtime. Method [`PARTNER_CALLBACK`] requires
//! `greeting` through the copied vtable and frames the outbound result as
//! `i32 little-endian status || reply bytes`, matching the demo's unpacking.

use dougong_abi::{ABI_VERSION, HostExports, PluginExports, STATUS_CAPACITY, STATUS_OK, write_out};

/// Method selector: requires `greeting` (method 0, the greeter's hello) with
/// the call payload and returns the framed outbound result.
pub const PARTNER_CALLBACK: u32 = 0;

/// The granted service this plugin requires through the dynamic channel, and
/// the method the greeter's `greeting` service answers with its hello bytes.
const GREETING_SERVICE: &[u8] = b"greeting";
const GREETING_HELLO: u32 = 0;

struct PartnerState {
    host: HostExports,
}

unsafe extern "C" fn partner_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract.
    if unsafe { write_out(b"grant-partner", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn partner_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract. The empty inject list is the
    // point: callbacks ride the runtime grant, not a static declaration.
    if unsafe {
        write_out(
            br#"{"name":"grant-partner","provide":["grant-callback"],"inject":[]}"#,
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

unsafe extern "C" fn partner_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of setup; copied by value.
    let host = unsafe { *host };
    Box::into_raw(Box::new(PartnerState { host })).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn partner_call(
    handle: *mut std::ffi::c_void,
    method: u32,
    data: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if handle.is_null() {
        return dougong_abi::STATUS_FAILED;
    }
    // SAFETY: live handle; the host serializes foreign entries per instance.
    let state = unsafe { &mut *handle.cast::<PartnerState>() };
    if method != PARTNER_CALLBACK {
        return dougong_abi::STATUS_FAILED;
    }
    let payload = if data.is_null() {
        &[]
    } else {
        // SAFETY: the vtable contract makes data readable for len bytes here.
        unsafe { std::slice::from_raw_parts(data, len) }
    };
    let (status, reply) = require_greeting(&state.host, payload);
    pack_framed(status, &reply, out, cap, out_len)
}

/// The descriptor listens to no events, so these entries are never invoked;
/// the export table still requires valid function pointers.
const unsafe extern "C" fn partner_on_event(
    _handle: *mut std::ffi::c_void,
    _event: u32,
    _mode: u32,
    _data: *const u8,
    _len: usize,
    _next_available: u32,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    dougong_abi::STATUS_FAILED
}

const unsafe extern "C" fn partner_on_next(
    _handle: *mut std::ffi::c_void,
    _event: u32,
    _chain_status: i32,
    _data: *const u8,
    _len: usize,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    dougong_abi::STATUS_FAILED
}

unsafe extern "C" fn partner_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: allocated by partner_setup and torn down exactly once.
    drop(unsafe { Box::from_raw(handle.cast::<PartnerState>()) });
}

/// Requires `greeting` through the copied vtable with the caller's payload,
/// growing the reply buffer on `STATUS_CAPACITY`.
fn require_greeting(host: &HostExports, payload: &[u8]) -> (i32, Vec<u8>) {
    let mut cap = 4096;
    loop {
        let mut buf = vec![0u8; cap];
        let mut len = 0usize;
        // SAFETY: the vtable is live until this activation's teardown
        // returns (the host keeps the activation alive that long); buffers
        // are caller-owned.
        let status = unsafe {
            (host.require)(
                host.host_handle,
                GREETING_SERVICE.as_ptr(),
                GREETING_SERVICE.len(),
                GREETING_HELLO,
                payload.as_ptr(),
                payload.len(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        if status != STATUS_CAPACITY {
            buf.truncate(len);
            return (status, buf);
        }
        cap *= 2;
    }
}

/// Packs `i32 little-endian status || payload` into the caller-owned out
/// buffer (the framing the demo's grant arm documents for replies).
fn pack_framed(status: i32, payload: &[u8], out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&status.to_le_bytes());
    framed.extend_from_slice(payload);
    // SAFETY: caller-owned buffer contract.
    if unsafe { write_out(&framed, out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

/// The export table every `load` resolves through
/// `DOUGONG_PLUGIN_EXPORTS`.
#[unsafe(no_mangle)]
pub static DOUGONG_PLUGIN_EXPORTS: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: partner_name,
    descriptor: partner_descriptor,
    setup: partner_setup,
    call: partner_call,
    on_event: partner_on_event,
    on_next: partner_on_next,
    teardown: partner_teardown,
};
