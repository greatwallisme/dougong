// Rust guideline compliant 2026-10-28
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
//!
//! # Outbound surface (ABI v4)
//!
//! `setup` copies the `HostExports` vtable and spawns one worker thread,
//! which synchronously requires the declared `greeter.oracle` service and
//! then publishes `greeter/ready` on the kernel bus. `teardown` joins the
//! worker before returning — the thread discipline the ABI contract
//! documents. Two more methods expose the outbound surface to hosts:
//!
//! - [`GREETER_WORKER_REPLY`]: the worker thread's stored `require` reply
//!   (empty until the worker has finished).
//! - [`GREETER_REQUIRE`]: an on-demand `require` issued from inside the call
//!   handler. Input is `name\0payload` (the first NUL separates name from
//!   payload; no NUL means an empty payload), output frames the outbound
//!   result as `i32 little-endian status || reply bytes`; failure statuses
//!   carry the host's JSON detail document as the reply bytes.
//! - [`GREETER_GRANT`]: an activation-scoped capability grant. Input is
//!   `target\0granted`, with the same status framing and no reply payload.

use std::io::Write;
use std::sync::{Arc, Mutex};

use dougong_abi::{ABI_VERSION, HostExports, PluginExports, write_out};

/// Method selector: returns the greeting bytes.
pub const GREETER_HELLO: u32 = 0;

/// Method selector: returns the worker thread's stored outbound `require`
/// reply (empty while the worker has not completed).
pub const GREETER_WORKER_REPLY: u32 = 1;

/// Method selector: issues an on-demand outbound `require` from inside the
/// call handler; see the crate documentation for the `name\0payload` input
/// and `i32 LE status || reply` output framing.
pub const GREETER_REQUIRE: u32 = 2;

/// Method selector: grants a target activation access to one owned service.
/// Input is `target\0granted`; output frames the grant status like
/// [`GREETER_REQUIRE`].
pub const GREETER_GRANT: u32 = 3;

/// Event id: `ping` — echo a pong.
pub const GREETER_EV_PING: u32 = 1;

/// Event id: `upper` — waterfall pull, upper-case the chain reply.
pub const GREETER_EV_UPPER: u32 = 2;

const GREETING: &[u8] = b"hello from a dougong dylib";
const PONG: &[u8] = b"\"pong from greeter-dylib\"";

/// The declared outbound dependency: an echo service (`oracle:<payload>`)
/// that every greeter-loading demo world provides in-process.
const ORACLE_SERVICE: &[u8] = b"greeter.oracle";

/// The event the worker thread publishes once its `require` completes.
const READY_EVENT: &[u8] = b"greeter/ready";

/// Fixed JSON document: the reply bytes never ride the event, so no JSON
/// escaping ever happens inside the plugin.
const READY_PAYLOAD: &[u8] = br#"{"source":"greeter-worker"}"#;

/// `Send` wrapper for the copied vtable: the ABI contract explicitly allows
/// all callable entries from any plugin thread, concurrently.
struct SendHost(HostExports);

// SAFETY: the vtable contract states the entries are callable from any
// thread concurrently; the host implementations are panic-contained.
unsafe impl Send for SendHost {}

/// Plugin-side growable `require`: retries with a doubled buffer whenever
/// the host asks for more room (the caller-owned buffer protocol).
fn plugin_require(
    host: &HostExports,
    service: &[u8],
    method: u32,
    payload: &[u8],
) -> (i32, Vec<u8>) {
    let mut cap = 4096_usize;
    loop {
        let mut buf = vec![0_u8; cap];
        let mut len = 0_usize;
        // SAFETY: the copied vtable is live until this activation's
        // `teardown` returns, and every caller below finishes before then
        // (`teardown` joins the worker); buffers are caller-owned.
        let status = unsafe {
            (host.require)(
                host.host_handle,
                service.as_ptr(),
                service.len(),
                method,
                payload.as_ptr(),
                payload.len(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        if status == dougong_abi::STATUS_CAPACITY {
            cap = cap.saturating_mul(2);
            continue;
        }
        buf.truncate(len);
        return (status, buf);
    }
}

/// The outbound demonstration body: one synchronous `require` from the
/// plugin's own thread, then one `emit`; the reply is parked for
/// [`GREETER_WORKER_REPLY`] to read back.
fn worker_main(host: &SendHost, sink: &Arc<Mutex<Option<Vec<u8>>>>) {
    let (_status, reply) = plugin_require(&host.0, ORACLE_SERVICE, 0, b"worker-hello");
    if let Ok(mut slot) = sink.lock() {
        *slot = Some(reply);
    }
    // SAFETY: the vtable is valid until `teardown` returns and `teardown`
    // joins this worker first; the payload is a fixed valid JSON document.
    let _ = unsafe {
        (host.0.emit)(
            host.0.host_handle,
            READY_EVENT.as_ptr(),
            READY_EVENT.len(),
            READY_PAYLOAD.as_ptr(),
            READY_PAYLOAD.len(),
        )
    };
}

/// Frames `status || payload` into the out buffer (the [`GREETER_REQUIRE`]
/// method contract).
fn pack_out(status: i32, payload: &[u8], out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&status.to_le_bytes());
    framed.extend_from_slice(payload);
    // SAFETY: host buffer contract (caller-owned out buffer).
    if unsafe { write_out(&framed, out, cap, out_len) } {
        dougong_abi::STATUS_OK
    } else {
        dougong_abi::STATUS_CAPACITY
    }
}

struct State {
    invocations: u32,
    /// Set between a `STATUS_DEFERRED` waterfall answer and the matching
    /// `on_next` call. The host serializes every foreign call per instance,
    /// so no other event can interleave here.
    pending_upper: bool,
    /// The vtable copied at `setup`; `GREETER_REQUIRE` uses it from inside
    /// the call handler (same thread, under the host's instance lock).
    host: HostExports,
    /// The outbound worker; `teardown` joins it before returning.
    worker: Option<std::thread::JoinHandle<()>>,
    /// Shared with the worker. The host's per-instance lock does NOT cover
    /// plugin-owned threads, so this slot carries its own mutex.
    worker_reply: Arc<Mutex<Option<Vec<u8>>>>,
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
    let descriptor = br#"{"name":"greeter","version":"0.4.0","provide":["greeting"],"inject":["greeter.oracle"],"listen":{"ping":1,"upper":2}}"#;
    // SAFETY: same host buffer contract as `name`.
    if unsafe { write_out(descriptor, out, cap, len) } {
        dougong_abi::STATUS_OK
    } else {
        dougong_abi::STATUS_CAPACITY
    }
}

unsafe extern "C" fn setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the host exports pointer is valid for the duration of `setup`;
    // the vtable is copied by value (v3 contract).
    let host = unsafe { *host };
    let worker_reply = Arc::new(Mutex::new(None));
    let worker = {
        let host = SendHost(host);
        let sink = Arc::clone(&worker_reply);
        std::thread::Builder::new()
            .name("greeter-worker".to_string())
            .spawn(move || worker_main(&host, &sink))
    };
    let Ok(worker) = worker else {
        // The outbound demonstration is half of this plugin's contract: a
        // host that cannot spawn the worker gets a loud initialization
        // failure, not a silently half-alive instance.
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(State {
        invocations: 0,
        pending_upper: false,
        host,
        worker: Some(worker),
        worker_reply,
    }))
    .cast::<std::ffi::c_void>()
}

unsafe extern "C" fn call(
    handle: *mut std::os::raw::c_void,
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
    // SAFETY: the host passes back a handle previously returned by `setup`
    // and not yet released via `teardown`.
    let state = unsafe { &mut *(handle.cast::<State>()) };
    let input = if data.is_null() {
        &[][..]
    } else {
        // SAFETY: `data` is valid for `len` readable bytes (host contract);
        // null pairs with length 0 and took the empty-slice branch above.
        unsafe { std::slice::from_raw_parts(data, len) }
    };
    match method {
        GREETER_HELLO => {
            state.invocations += 1;
            // SAFETY: same host buffer contract as `name`.
            if unsafe { write_out(GREETING, out, cap, out_len) } {
                dougong_abi::STATUS_OK
            } else {
                dougong_abi::STATUS_CAPACITY
            }
        }
        GREETER_WORKER_REPLY => {
            let reply = state
                .worker_reply
                .lock()
                .ok()
                .and_then(|slot| slot.clone())
                .unwrap_or_default();
            // SAFETY: host buffer contract.
            if unsafe { write_out(&reply, out, cap, out_len) } {
                dougong_abi::STATUS_OK
            } else {
                dougong_abi::STATUS_CAPACITY
            }
        }
        GREETER_REQUIRE => {
            let (service, payload) = input.iter().position(|byte| *byte == 0).map_or_else(
                || (input, &[][..]),
                |split| (&input[..split], &input[split + 1..]),
            );
            let (status, reply) = plugin_require(&state.host, service, 0, payload);
            pack_out(status, &reply, out, cap, out_len)
        }
        GREETER_GRANT => {
            let (target, granted) = input.iter().position(|byte| *byte == 0).map_or_else(
                || (input, &[][..]),
                |split| (&input[..split], &input[split + 1..]),
            );
            // SAFETY: this activation is live and both input slices remain
            // readable for the synchronous registration call.
            let status = unsafe {
                (state.host.grant)(
                    state.host.host_handle,
                    target.as_ptr(),
                    target.len(),
                    granted.as_ptr(),
                    granted.len(),
                )
            };
            pack_out(status, b"", out, cap, out_len)
        }
        _ => dougong_abi::STATUS_FAILED,
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
    // Thread discipline: the worker is joined before teardown returns, so no
    // plugin-owned thread outlives the vtable it calls through. The worker
    // never touches this instance's lock (its `require` targets another
    // plugin), so the join cannot deadlock against the host-held lock.
    if let Some(worker) = state.worker {
        let _joined = worker.join();
    }
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
