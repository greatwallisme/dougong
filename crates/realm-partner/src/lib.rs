// Rust guideline compliant 2026-11-05
//! Example realm-spawning dougong plugin as a pure C-ABI cdylib (ABI v6).
//!
//! Depends only on `dougong-abi`. The `outbound_demo` realm arm (D33) drives
//! it through three service methods; each one performs its vtable call on a
//! short-lived plugin-owned thread, because `realm_spawn` and `realm_retire`
//! reject foreign entry frames. Replies are framed as
//! `i32 little-endian status || reply bytes`, matching the demo's unpacking.

use dougong_abi::{
    ABI_VERSION, HostExports, PluginExports, REALM_SPAWN_REPLY_MIN, STATUS_CAPACITY, STATUS_FAILED,
    STATUS_OK, write_out,
};

/// Method selector: spawns the artifact whose UTF-8 path is the payload into
/// a fresh realm; replies with the host's JSON spawn document.
pub const PARTNER_SPAWN: u32 = 0;
/// Method selector: payload = realm label (u64 LE) || service name; calls
/// that service's method 0 with an empty body through `realm_require`.
pub const PARTNER_REQUIRE: u32 = 1;
/// Method selector: payload = child handle (u64 LE); retires that child.
pub const PARTNER_RETIRE: u32 = 2;

struct PartnerState {
    host: SendHost,
}

/// Copy of the host vtable handed to short-lived plugin-owned threads.
#[derive(Clone, Copy)]
struct SendHost(HostExports);

// SAFETY: the vtable contract states every entry is callable from any
// plugin-owned thread concurrently until this activation's teardown returns;
// each use below joins its thread before the call that spawned it returns.
unsafe impl Send for SendHost {}

unsafe extern "C" fn partner_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract.
    if unsafe { write_out(b"realm-partner", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn partner_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract. The empty inject list is the
    // point: `realm_require` needs no static declaration for its children.
    if unsafe {
        write_out(
            br#"{"name":"realm-partner","provide":["realm-spawner"],"inject":[]}"#,
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
    Box::into_raw(Box::new(PartnerState {
        host: SendHost(host),
    }))
    .cast::<std::ffi::c_void>()
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
        return STATUS_FAILED;
    }
    // SAFETY: live handle; the host serializes foreign entries per instance.
    let state = unsafe { &*handle.cast::<PartnerState>() };
    let payload = if data.is_null() {
        &[]
    } else {
        // SAFETY: the vtable contract makes data readable for len bytes here.
        unsafe { std::slice::from_raw_parts(data, len) }
    }
    .to_vec();
    let host = state.host;
    let (status, reply) = match method {
        PARTNER_SPAWN => on_plugin_thread(move || spawn(&host, &payload)),
        PARTNER_REQUIRE => on_plugin_thread(move || require(&host, &payload)),
        PARTNER_RETIRE => on_plugin_thread(move || retire(&host, &payload)),
        _ => return STATUS_FAILED,
    };
    pack_framed(status, &reply, out, cap, out_len)
}

/// Runs `work` on a fresh plugin-owned thread and joins it; a panicking
/// worker reports `STATUS_FAILED` instead of unwinding across the boundary.
///
/// NOTE the safety condition this example relies on: the calling service
/// entry holds this plugin's per-instance lock while joining the worker, so
/// a spawned child whose `setup` (or teardown) REQUIRES THIS PLUGIN's
/// services would park on that lock and deadlock. That is safe here because
/// the demo's children require the greeter's services, never this
/// plugin's. A real plugin whose children may require it back must initiate
/// spawns from threads started in `setup` (which hold no instance lock),
/// not from inside service calls.
fn on_plugin_thread(work: impl FnOnce() -> (i32, Vec<u8>) + Send + 'static) -> (i32, Vec<u8>) {
    std::thread::spawn(work)
        .join()
        .unwrap_or((STATUS_FAILED, Vec::new()))
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
    STATUS_FAILED
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
    STATUS_FAILED
}

unsafe extern "C" fn partner_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: allocated by partner_setup and torn down exactly once; every
    // plugin-owned thread was joined inside the call that spawned it.
    drop(unsafe { Box::from_raw(handle.cast::<PartnerState>()) });
}

/// `realm_spawn` into a fresh realm; the buffer honors the documented
/// minimum so the host never answers with `STATUS_CAPACITY`.
fn spawn(host: &SendHost, path: &[u8]) -> (i32, Vec<u8>) {
    let mut buf = vec![0_u8; 4096.max(REALM_SPAWN_REPLY_MIN)];
    let mut len = 0_usize;
    // SAFETY: the vtable is live until this activation's teardown returns;
    // buffers are caller-owned.
    let status = unsafe {
        (host.0.realm_spawn)(
            host.0.host_handle,
            0,
            path.as_ptr(),
            path.len(),
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut len,
        )
    };
    buf.truncate(len);
    (status, buf)
}

/// `realm_require` against the realm named by the payload's first eight
/// bytes, growing the reply buffer on `STATUS_CAPACITY`.
fn require(host: &SendHost, payload: &[u8]) -> (i32, Vec<u8>) {
    let Some((realm, service)) = payload.split_first_chunk::<8>() else {
        return (STATUS_FAILED, Vec::new());
    };
    let realm = u64::from_le_bytes(*realm);
    let mut cap = 4096_usize;
    loop {
        let mut buf = vec![0_u8; cap];
        let mut len = 0_usize;
        // SAFETY: the vtable is live; buffers are caller-owned.
        let status = unsafe {
            (host.0.realm_require)(
                host.0.host_handle,
                realm,
                service.as_ptr(),
                service.len(),
                0,
                std::ptr::null(),
                0,
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        if status != STATUS_CAPACITY {
            buf.truncate(len);
            return (status, buf);
        }
        cap = cap.saturating_mul(2);
    }
}

/// `realm_retire` of the child named by the payload.
fn retire(host: &SendHost, payload: &[u8]) -> (i32, Vec<u8>) {
    let Ok(child) = <[u8; 8]>::try_from(payload) else {
        return (STATUS_FAILED, Vec::new());
    };
    // SAFETY: the vtable is live; the entry reads no caller buffers.
    let status = unsafe { (host.0.realm_retire)(host.0.host_handle, u64::from_le_bytes(child)) };
    (status, Vec::new())
}

/// Packs `i32 little-endian status || payload` into the caller-owned out
/// buffer (the framing the demo's realm arm documents for replies). A reply
/// larger than the host's current buffer makes the HOST retry the whole
/// `call` from 4096 bytes, re-executing the method — the demo's replies are
/// tens of bytes, but non-idempotent methods should size their replies well
/// under 4 KiB (same framing caveat as the grant-partner example).
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
