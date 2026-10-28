// Rust guideline compliant 2026-10-28
//! E1 test matrix for the ABI v3 outbound surface and ABI v4 grant surface.
//!
//! The fixtures are hand-written "real" plugins over static export tables:
//! they copy the `HostExports` vtable at `setup` and call back through it
//! from handler threads and from plugin-owned threads, exactly as a
//! third-party `cdylib` would. Framing between test and plugin is
//! `4-byte LE status || payload` so the test can assert host status codes.
//!
//! Every test builds its own `Context` root, so parallel test threads share
//! nothing but the stateless static tables.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use dougong::{
    Context, ContextOptions, Error, FiberHandle, Injection, Invoker, LogLevel, LogRecord, Logger,
    LoggerHandle, Plugin, Result,
};
use dougong_abi::{
    HostExports, PluginExports, STATUS_CAPACITY, STATUS_FAILED, STATUS_INACTIVE, STATUS_OK,
    STATUS_REENTRANCY, STATUS_UNDECLARED, write_out,
};
use serde_json::Value;

use super::{DylibInvoke, EntryGuard, Exports, Handle, InstanceLock, from_static, host_require};

const STATUS_INVALID_PAYLOAD: i32 = dougong_abi::STATUS_INVALID_PAYLOAD;
const STATUS_INVALID_EVENT: i32 = dougong_abi::STATUS_INVALID_EVENT;

// ---------------------------------------------------------------------------
// Framing helpers
// ---------------------------------------------------------------------------

/// Packs `status || payload` into the out buffer (test framing).
fn pack(status: i32, payload: &[u8], out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&status.to_le_bytes());
    framed.extend_from_slice(payload);
    // SAFETY: host buffer contract (caller-owned out buffer).
    if unsafe { write_out(&framed, out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

/// Unpacks the `status || payload` framing on the test side.
fn unpack(bytes: &[u8]) -> (i32, &[u8]) {
    let (head, payload) = bytes.split_at(4);
    (
        i32::from_le_bytes(head.try_into().expect("framing prefix")),
        payload,
    )
}

/// `Send` wrapper for a copied vtable: the ABI contract (A5) explicitly
/// allows `require`/`emit` from any plugin thread.
struct SendHost(HostExports);
// SAFETY: the vtable contract states the entries are callable from any
// thread concurrently; the host implementations are `Send + Sync`.
unsafe impl Send for SendHost {}

impl SendHost {
    /// Calls `require` through the copied vtable (whole-struct method so
    /// `move` closures capture the `Send` wrapper, not the raw field).
    fn require(&self, service: &[u8], method: u32, payload: &[u8]) -> (i32, Vec<u8>) {
        plugin_require(&self.0, service, method, payload)
    }

    /// Calls `grant` through the copied vtable.
    fn grant(&self, target: &[u8], granted: &[u8]) -> i32 {
        plugin_grant(&self.0, target, granted)
    }
}

/// Plugin-side grant: the v4 registration primitive has no out-buffer.
fn plugin_grant(host: &HostExports, target: &[u8], granted: &[u8]) -> i32 {
    // SAFETY: the vtable is live and both names are readable byte strings.
    unsafe {
        (host.grant)(
            host.host_handle,
            target.as_ptr(),
            target.len(),
            granted.as_ptr(),
            granted.len(),
        )
    }
}

/// Plugin-side growable `require`: retries with a doubled buffer whenever
/// the host asks for more room (the v2 caller-owned buffer protocol).
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
        // SAFETY: the vtable is live (fixtures never outlive their
        // activation); buffers are caller-owned.
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
        if status == STATUS_CAPACITY {
            cap = cap.saturating_mul(2);
            continue;
        }
        buf.truncate(len);
        return (status, buf);
    }
}

// ---------------------------------------------------------------------------
// Consumer fixture: provides svc-a, injects svc-b / svc-mid / svc-host
// ---------------------------------------------------------------------------

const CONSUMER_CALL_B: u32 = 0;
const CONSUMER_REQUIRE_NAMED: u32 = 1;
const CONSUMER_REQUIRE_THREAD_NAMED: u32 = 2;
const CONSUMER_REQUIRE_THREAD_SLOW: u32 = 3;
const CONSUMER_REQUIRE_CYCLE: u32 = 4;
const CONSUMER_SETUP_RESULT: u32 = 5;
const CONSUMER_ACTIVATION: u32 = 6;
const CONSUMER_ECHO: u32 = 7;
const CONSUMER_SPAWN_HAMMER: u32 = 8;
const CONSUMER_GRANT: u32 = 9;
const CONSUMER_SETUP_GRANT: u32 = 10;
const CONSUMER_GRANT_HAMMER: u32 = 11;

static CONSUMER_ACTIVATIONS: AtomicU64 = AtomicU64::new(0);

struct ConsumerState {
    host: HostExports,
    /// Result of the require issued *during* `setup` (E1.10 probe).
    setup_probe: (i32, Vec<u8>),
    /// Result of the optional grant issued during setup (ABI v4 E1.12).
    setup_grant: i32,
    /// Identity of this activation: a monotonic setup token. An address
    /// would not do — the allocator may legally reuse the freed state's
    /// address for the next activation, which made a pointer comparison
    /// flaky.
    activation: u64,
}

unsafe extern "C" fn consumer_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"fake-consumer", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn consumer_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"fake-consumer","provide":["svc-a","svc-a-alt"],"inject":["svc-b","svc-mid","svc-host"]}"#,
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

unsafe extern "C" fn grant_consumer_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"fake-grant-consumer","provide":["svc-a"],"inject":["svc-b"]}"#,
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

unsafe extern "C" fn consumer_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the host exports pointer is valid for the duration of `setup`;
    // the vtable is copied by value (v3 contract).
    let host = unsafe { *host };
    // E1.10 probe: require a committed provider during setup (G5/B5).
    let setup_probe = plugin_require(&host, b"svc-b", 0, b"setup-probe");
    // Monotonic per-activation token: unique per setup run (Relaxed is
    // sufficient — uniqueness only, no ordering consumers).
    let activation = CONSUMER_ACTIVATIONS.fetch_add(1, Ordering::Relaxed) + 1;
    Box::into_raw(Box::new(ConsumerState {
        host,
        setup_probe,
        setup_grant: STATUS_OK,
        activation,
    }))
    .cast::<std::ffi::c_void>()
}

/// Grant-on-setup variant used by the reactivation self-healing arm.
unsafe extern "C" fn grant_consumer_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of setup; copied by value.
    let host = unsafe { *host };
    let setup_probe = plugin_require(&host, b"svc-b", 0, b"setup-probe");
    let setup_grant = plugin_grant(&host, b"svc-b", b"svc-a");
    let activation = CONSUMER_ACTIVATIONS.fetch_add(1, Ordering::Relaxed) + 1;
    Box::into_raw(Box::new(ConsumerState {
        host,
        setup_probe,
        setup_grant,
        activation,
    }))
    .cast::<std::ffi::c_void>()
}

fn consumer_grant_hammer(host: HostExports) -> (i32, &'static [u8]) {
    let mut workers = Vec::new();
    for _ in 0..4 {
        let host = SendHost(host);
        workers.push(std::thread::spawn(move || {
            (0..4).all(|_| host.grant(b"svc-b", b"svc-a") == STATUS_OK)
        }));
    }
    let ok = workers
        .into_iter()
        .all(|worker| worker.join().expect("grant hammer thread"));
    (if ok { STATUS_OK } else { STATUS_FAILED }, b"16")
}

unsafe extern "C" fn consumer_call(
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
    // SAFETY: live handle per the export-table contract; the host serializes
    // foreign entries per instance.
    let state = unsafe { &mut *handle.cast::<ConsumerState>() };
    // SAFETY: vtable input contract.
    let input = unsafe { super::input_slice(data, len) };
    match method {
        CONSUMER_CALL_B => {
            let (status, reply) = plugin_require(&state.host, b"svc-b", 0, input);
            pack(status, &reply, out, cap, out_len)
        }
        CONSUMER_REQUIRE_NAMED => {
            let (status, reply) = plugin_require(&state.host, input, 0, b"probe");
            pack(status, &reply, out, cap, out_len)
        }
        CONSUMER_REQUIRE_THREAD_NAMED | CONSUMER_REQUIRE_THREAD_SLOW => {
            let host = SendHost(state.host);
            let name = if method == CONSUMER_REQUIRE_THREAD_SLOW {
                b"svc-b".to_vec()
            } else {
                input.to_vec()
            };
            let sub_method = u32::from(method == CONSUMER_REQUIRE_THREAD_SLOW);
            let worker = std::thread::spawn(move || host.require(&name, sub_method, b"probe"));
            match worker.join() {
                Ok((status, reply)) => pack(status, &reply, out, cap, out_len),
                Err(_) => STATUS_FAILED,
            }
        }
        CONSUMER_REQUIRE_CYCLE => {
            let (status, reply) = plugin_require(&state.host, b"svc-mid", 9, b"");
            pack(status, &reply, out, cap, out_len)
        }
        CONSUMER_SETUP_RESULT => {
            let (status, reply) = &state.setup_probe;
            pack(*status, reply, out, cap, out_len)
        }
        CONSUMER_ACTIVATION => pack(
            0,
            state.activation.to_le_bytes().as_slice(),
            out,
            cap,
            out_len,
        ),
        CONSUMER_ECHO => {
            // SAFETY: host buffer contract.
            if unsafe { write_out(input, out, cap, out_len) } {
                STATUS_OK
            } else {
                STATUS_CAPACITY
            }
        }
        CONSUMER_GRANT => {
            let (target, granted) = input.iter().position(|byte| *byte == 0).map_or_else(
                || (input, &[][..]),
                |split| (&input[..split], &input[split + 1..]),
            );
            pack(
                plugin_grant(&state.host, target, granted),
                b"",
                out,
                cap,
                out_len,
            )
        }
        CONSUMER_SETUP_GRANT => pack(state.setup_grant, b"", out, cap, out_len),
        CONSUMER_GRANT_HAMMER => {
            let (status, payload) = consumer_grant_hammer(state.host);
            pack(status, payload, out, cap, out_len)
        }
        CONSUMER_SPAWN_HAMMER => {
            // Four plugin-owned threads concurrently requiring the slow
            // target method: the target's per-instance lock must serialize
            // them (its `max_seen` never exceeds 1).
            let mut workers = Vec::new();
            for _ in 0..4 {
                let host = SendHost(state.host);
                workers.push(std::thread::spawn(move || {
                    host.require(b"svc-b", 1, b"hammer")
                }));
            }
            let mut replies = Vec::new();
            for worker in workers {
                match worker.join() {
                    Ok((status, reply)) => replies.push(format!("{status}:{}", reply.len())),
                    Err(_) => return STATUS_FAILED,
                }
            }
            pack(0, replies.join(",").as_bytes(), out, cap, out_len)
        }
        _ => STATUS_FAILED,
    }
}

unsafe extern "C" fn consumer_on_event(
    _handle: *mut std::ffi::c_void,
    _event: u32,
    _mode: u32,
    _data: *const u8,
    _len: usize,
    _next: u32,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    STATUS_FAILED
}

unsafe extern "C" fn consumer_on_next(
    _handle: *mut std::ffi::c_void,
    _event: u32,
    _status: i32,
    _data: *const u8,
    _len: usize,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    STATUS_FAILED
}

unsafe extern "C" fn consumer_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the handle came from `consumer_setup` and is torn down once.
    let state = unsafe { Box::from_raw(handle.cast::<ConsumerState>()) };
    // Emit during teardown is legal (no instance lock involved): the event
    // proves to the test that this teardown ran (E1.9/E1.11).
    let name = b"consumer-teardown";
    // SAFETY: teardown is a live-activation entry; the vtable is valid until
    // this export returns (A4).
    let _ = unsafe {
        (state.host.emit)(
            state.host.host_handle,
            name.as_ptr(),
            name.len(),
            b"null".as_ptr(),
            4,
        )
    };
}

static CONSUMER_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: consumer_name,
    descriptor: consumer_descriptor,
    setup: consumer_setup,
    call: consumer_call,
    on_event: consumer_on_event,
    on_next: consumer_on_next,
    teardown: consumer_teardown,
};

static GRANT_CONSUMER_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: consumer_name,
    descriptor: grant_consumer_descriptor,
    setup: grant_consumer_setup,
    call: consumer_call,
    on_event: consumer_on_event,
    on_next: consumer_on_next,
    teardown: consumer_teardown,
};

// ---------------------------------------------------------------------------
// Target fixture: provides svc-b (echo / slow / stats)
// ---------------------------------------------------------------------------

const TARGET_REQUIRE_GRANTED: u32 = 3;
const TARGET_REQUIRE_GRANTED_HAMMER: u32 = 4;
const TARGET_REQUIRE_NAMED: u32 = 5;

struct TargetState {
    host: HostExports,
    in_call: u32,
    max_seen: u32,
    calls: u32,
}

unsafe extern "C" fn target_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"fake-target", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn target_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"fake-target","provide":["svc-b"],"inject":[]}"#,
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

unsafe extern "C" fn target_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of `setup`; copied by value.
    let host = unsafe { *host };
    Box::into_raw(Box::new(TargetState {
        host,
        in_call: 0,
        max_seen: 0,
        calls: 0,
    }))
    .cast::<std::ffi::c_void>()
}

unsafe extern "C" fn target_call(
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
    // SAFETY: live handle; serialized by the host per-instance lock.
    let state = unsafe { &mut *handle.cast::<TargetState>() };
    // SAFETY: vtable input contract.
    let input = unsafe { super::input_slice(data, len) };
    state.calls += 1;
    // The target is a plain service: its replies are raw payloads (the
    // require/call channel is opaque bytes; only test-driven fixture
    // services pack a status prefix).
    let reply: Vec<u8> = match method {
        0 => {
            // Self-identifying echo: the instance address distinguishes
            // same-table instances (realm tests).
            let mut bytes = format!("target@{state:p}:").into_bytes();
            bytes.extend_from_slice(input);
            bytes
        }
        1 => {
            state.in_call += 1;
            state.max_seen = state.max_seen.max(state.in_call);
            std::thread::sleep(Duration::from_millis(50));
            state.in_call -= 1;
            b"slow-done".to_vec()
        }
        2 => format!("{},{}", state.calls, state.max_seen).into_bytes(),
        TARGET_REQUIRE_GRANTED => {
            let (status, reply) = plugin_require(&state.host, b"svc-a", CONSUMER_ECHO, input);
            return pack(status, &reply, out, cap, out_len);
        }
        TARGET_REQUIRE_GRANTED_HAMMER => {
            let mut workers = Vec::new();
            for _ in 0..4 {
                let host = SendHost(state.host);
                workers.push(std::thread::spawn(move || {
                    (0..4).all(|_| {
                        let (status, reply) = host.require(b"svc-a", CONSUMER_ECHO, b"hammer");
                        status == STATUS_OK && reply == b"hammer"
                    })
                }));
            }
            let ok = workers
                .into_iter()
                .all(|worker| worker.join().expect("require hammer thread"));
            return pack(
                if ok { STATUS_OK } else { STATUS_FAILED },
                b"16",
                out,
                cap,
                out_len,
            );
        }
        TARGET_REQUIRE_NAMED => {
            let (status, reply) = plugin_require(&state.host, input, CONSUMER_ECHO, b"probe");
            return pack(status, &reply, out, cap, out_len);
        }
        _ => return STATUS_FAILED,
    };
    // SAFETY: host buffer contract.
    if unsafe { write_out(&reply, out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn target_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: handle from `target_setup`, torn down once.
    let state = unsafe { Box::from_raw(handle.cast::<TargetState>()) };
    let name = b"target-teardown";
    // SAFETY: teardown is a live-activation entry; vtable valid until return.
    let _ = unsafe {
        (state.host.emit)(
            state.host.host_handle,
            name.as_ptr(),
            name.len(),
            b"null".as_ptr(),
            4,
        )
    };
}

static TARGET_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: target_name,
    descriptor: target_descriptor,
    setup: target_setup,
    call: target_call,
    on_event: consumer_on_event,
    on_next: consumer_on_next,
    teardown: target_teardown,
};

// ---------------------------------------------------------------------------
// Emitter fixture: provides svc-emitter, listens evt-x, emit methods
// ---------------------------------------------------------------------------

const EMITTER_EMIT: u32 = 0;
const EMITTER_EMIT_BAD_JSON: u32 = 1;
const EMITTER_HITS: u32 = 2;

struct EmitterState {
    host: HostExports,
    hits: u32,
}

unsafe extern "C" fn emitter_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"fake-emitter", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn emitter_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"fake-emitter","provide":["svc-emitter"],"inject":[],"listen":{"evt-x":7}}"#,
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

unsafe extern "C" fn emitter_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of `setup`; copied by value.
    let host = unsafe { *host };
    Box::into_raw(Box::new(EmitterState { host, hits: 0 })).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn emitter_call(
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
    // SAFETY: live handle; host-serialized.
    let state = unsafe { &mut *handle.cast::<EmitterState>() };
    // SAFETY: vtable input contract.
    let name = unsafe { super::input_slice(data, len) };
    match method {
        EMITTER_EMIT | EMITTER_EMIT_BAD_JSON => {
            let payload: &[u8] = if method == EMITTER_EMIT {
                b"42"
            } else {
                b"not-json"
            };
            // SAFETY: live activation; inputs valid for their lengths.
            let status = unsafe {
                (state.host.emit)(
                    state.host.host_handle,
                    name.as_ptr(),
                    name.len(),
                    payload.as_ptr(),
                    payload.len(),
                )
            };
            pack(status, b"", out, cap, out_len)
        }
        EMITTER_HITS => pack(0, state.hits.to_string().as_bytes(), out, cap, out_len),
        _ => STATUS_FAILED,
    }
}

unsafe extern "C" fn emitter_on_event(
    handle: *mut std::ffi::c_void,
    event: u32,
    _mode: u32,
    _data: *const u8,
    _len: usize,
    _next: u32,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    if handle.is_null() || event != 7 {
        return STATUS_FAILED;
    }
    // SAFETY: live handle; host-serialized.
    let state = unsafe { &mut *handle.cast::<EmitterState>() };
    state.hits += 1;
    STATUS_OK
}

unsafe extern "C" fn emitter_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: handle from `emitter_setup`, torn down once.
    drop(unsafe { Box::from_raw(handle.cast::<EmitterState>()) });
}

static EMITTER_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: emitter_name,
    descriptor: emitter_descriptor,
    setup: emitter_setup,
    call: emitter_call,
    on_event: emitter_on_event,
    on_next: consumer_on_next,
    teardown: emitter_teardown,
};

// ---------------------------------------------------------------------------
// In-process providers
// ---------------------------------------------------------------------------

/// svc-host: echoes back "host:<payload>".
struct HostSvc;

impl dougong::Invoke for HostSvc {
    fn call(&self, _method: u32, payload: Vec<u8>) -> dougong::BoxFuture<Result<Vec<u8>>> {
        Box::pin(async move { Ok([b"host:".as_ref(), &payload].concat()) })
    }
}

/// svc-mid: echoes "mid:<payload>"; method 9 chains into svc-a (used to
/// build the A -> mid -> A reentrancy cycle without a fiber-level
/// declaration cycle, which would park both fibers forever).
struct MidSvc {
    ctx: Context,
}

impl dougong::Invoke for MidSvc {
    fn call(&self, method: u32, payload: Vec<u8>) -> dougong::BoxFuture<Result<Vec<u8>>> {
        if method == 9 {
            let Some(invoker) = self.ctx.get::<Invoker>("svc-a") else {
                return Box::pin(async { Err(Error::msg("svc-a missing")) });
            };
            return invoker.call(0, payload);
        }
        Box::pin(async move { Ok([b"mid:".as_ref(), &payload].concat()) })
    }
}

// ---------------------------------------------------------------------------
// World bootstrap
// ---------------------------------------------------------------------------

struct World {
    consumer: FiberHandle,
    target: FiberHandle,
    _plugins: Vec<Arc<dyn Plugin>>,
}

async fn start(ctx: &Context, plugin: Arc<dyn Plugin>) -> FiberHandle {
    super::arm_deadlock_watchdog();
    let handle = ctx.plugin(plugin, None).expect("spawn");
    tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("activation settles")
        .expect("activation ok");
    handle
}

/// Boots the standard topology: svc-host (in-process), svc-mid (in-process),
/// svc-b (dylib target), then the dylib consumer. Start order matters: the
/// consumer's setup-time probe requires svc-b to be committed already.
async fn boot_world(ctx: &Context) -> World {
    let host_svc = dougong::plugin("host-svc", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-host", Invoker::new(Arc::new(HostSvc)))
                .await?;
            Ok(())
        })
    });
    start(ctx, host_svc).await;
    let mid = dougong::plugin("mid", vec![], |ctx, _config| {
        Box::pin(async move {
            let svc = MidSvc { ctx: ctx.clone() };
            ctx.provide("svc-mid", Invoker::new(Arc::new(svc))).await?;
            Ok(())
        })
    });
    start(ctx, mid).await;
    let target: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target table"));
    let target = start(ctx, target).await;
    let consumer_plugin: Arc<dyn Plugin> =
        Arc::new(from_static(&CONSUMER_TABLE).expect("consumer table"));
    let consumer = start(ctx, Arc::clone(&consumer_plugin)).await;
    World {
        consumer,
        target,
        _plugins: vec![consumer_plugin],
    }
}

/// Calls a service through the dynamic channel with a deadlock guardrail,
/// then unpacks the test framing.
async fn call_framed(ctx: &Context, service: &str, method: u32, payload: &[u8]) -> (i32, Vec<u8>) {
    let bytes = call_raw(ctx, service, method, payload).await;
    let (status, payload) = unpack(&bytes);
    (status, payload.to_vec())
}

/// Calls a service whose replies are raw payloads (no test framing).
async fn call_raw(ctx: &Context, service: &str, method: u32, payload: &[u8]) -> Vec<u8> {
    let invoker = ctx.get::<Invoker>(service).expect("service committed");
    tokio::time::timeout(
        Duration::from_secs(5),
        invoker.call(method, payload.to_vec()),
    )
    .await
    .expect("no deadlock")
    .expect("call ok")
}

/// Polls `check` until it holds or the deadline passes (event arrivals are
/// asynchronous by design).
async fn wait_until(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Registers a counting listener for `name` on the root fiber.
async fn count_listener(ctx: &Context, name: &str) -> Arc<AtomicU64> {
    let hits = Arc::new(AtomicU64::new(0));
    let inner = Arc::clone(&hits);
    let listener: dougong::Listener = Arc::new(move |_ctx, _payload, _next| {
        let hits = Arc::clone(&inner);
        Box::pin(async move {
            hits.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        })
    });
    ctx.on(name, listener).await.expect("listener registers");
    hits
}

// ---------------------------------------------------------------------------
// E1.1: own-thread require reaches in-process and dylib providers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_thread_require_reaches_in_process_and_dylib_providers() {
    let ctx = Context::new();
    let world = boot_world(&ctx).await;

    let (status, payload) =
        call_framed(&ctx, "svc-a", CONSUMER_REQUIRE_THREAD_NAMED, b"svc-host").await;
    assert_eq!(status, STATUS_OK);
    assert_eq!(payload, b"host:probe");

    let (status, payload) =
        call_framed(&ctx, "svc-a", CONSUMER_REQUIRE_THREAD_NAMED, b"svc-b").await;
    assert_eq!(status, STATUS_OK);
    let text = String::from_utf8(payload).expect("utf8 reply");
    assert!(
        text.starts_with("target@"),
        "dylib provider answered: {text}"
    );
    assert!(text.ends_with(":probe"));

    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// E1.2: capability and liveness arms through the real FFI entry
// ---------------------------------------------------------------------------

/// Builds a detached activation handle for direct vtable drives.
fn bare_activation(ctx: &Context, inject: &[&str]) -> *mut std::ffi::c_void {
    bare_activation_with_provide(ctx, inject, &[])
}

/// Builds a detached activation with an explicit grant-ownership set.
fn bare_activation_with_provide(
    ctx: &Context,
    inject: &[&str],
    provide: &[&str],
) -> *mut std::ffi::c_void {
    super::arm_deadlock_watchdog();
    Arc::into_raw(Arc::new(super::Activation {
        ctx: ctx.clone(),
        rt: tokio::runtime::Handle::current(),
        instance_id: super::next_instance_id(),
        inject: inject.iter().map(|name| (*name).to_string()).collect(),
        provide: provide.iter().map(|name| (*name).to_string()).collect(),
    }))
    .cast_mut()
    .cast::<std::ffi::c_void>()
}

/// Drives `host_require` and returns (status, detail-or-payload).
unsafe fn raw_require(
    host: *mut std::ffi::c_void,
    service: &[u8],
    method: u32,
    payload: &[u8],
) -> (i32, Vec<u8>) {
    let mut buf = vec![0_u8; 4096];
    let mut len = 0_usize;
    // SAFETY: `host` names a live activation built by `bare_activation`;
    // buffers are caller-owned.
    let status = unsafe {
        host_require(
            host,
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
    buf.truncate(len);
    (status, buf)
}

/// Drives `host_grant` and returns its status-only ABI result.
unsafe fn raw_grant(host: *mut std::ffi::c_void, target: &[u8], granted: &[u8]) -> i32 {
    // SAFETY: `host` names a live activation and both byte strings are valid.
    unsafe {
        super::host_grant(
            host,
            target.as_ptr(),
            target.len(),
            granted.as_ptr(),
            granted.len(),
        )
    }
}

/// Reclaims a detached activation and runs its grant cleanup.
unsafe fn reclaim_activation(host: *mut std::ffi::c_void) {
    // SAFETY: each detached raw activation has exactly one reclaim and no
    // subsequent vtable use.
    unsafe { super::ActivationPtr(host).reclaim() };
}

#[tokio::test]
async fn require_enforces_capability_and_liveness() {
    let ctx = Context::new();
    let host_svc = dougong::plugin("host-svc", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-host", Invoker::new(Arc::new(HostSvc)))
                .await?;
            Ok(())
        })
    });
    start(&ctx, host_svc).await;

    let raw = bare_activation(&ctx, &["svc-host", "svc-missing"]);

    // Undeclared service -> STATUS_UNDECLARED with a JSON detail document.
    // SAFETY: `raw` names the live activation above.
    let (status, detail) = unsafe { raw_require(raw, b"svc-other", 0, b"") };
    assert_eq!(status, STATUS_UNDECLARED);
    assert_eq!(
        detail,
        br#"{"class":"logic","code":"undeclared_dependency","message":"cannot get property \"svc-other\" without inject"}"#
    );
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "undeclared_dependency");

    // Non-UTF-8 service name cannot be declared -> same arm.
    // SAFETY: same live activation.
    let (status, _) = unsafe { raw_require(raw, &[0xFF, 0xFE], 0, b"") };
    assert_eq!(status, STATUS_UNDECLARED);

    // Declared but never provided -> STATUS_INACTIVE.
    // SAFETY: same live activation.
    let (status, detail) = unsafe { raw_require(raw, b"svc-missing", 0, b"") };
    assert_eq!(status, STATUS_INACTIVE);
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "inactive_dependency");

    // Declared and committed -> STATUS_OK, payload passthrough.
    // SAFETY: same live activation.
    let (status, payload) = unsafe { raw_require(raw, b"svc-host", 0, b"hi") };
    assert_eq!(status, STATUS_OK);
    assert_eq!(payload, b"host:hi");

    // Same call from a plugin-owned thread (A5).
    let shared = SendHost(HostExports {
        host_handle: raw,
        require: host_require,
        emit: super::host_emit,
        grant: super::host_grant,
    });
    let outcome = std::thread::spawn(move || shared.require(b"svc-host", 0, b"t"))
        .join()
        .expect("plugin thread ok");
    assert_eq!(outcome.0, STATUS_OK);

    // SAFETY: unique reclaim of the activation created above; no vtable call
    // happens afterwards.
    drop(unsafe { Arc::from_raw(raw.cast::<super::Activation>()) });
    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// E1.3 + E1.5: legal nested chain, and same-thread self-reentry rejection
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_require_inside_call_handler_succeeds() {
    let ctx = Context::new();
    let world = boot_world(&ctx).await;
    let (status, payload) = call_framed(&ctx, "svc-a", CONSUMER_CALL_B, b"nested").await;
    assert_eq!(status, STATUS_OK);
    let text = String::from_utf8(payload).expect("utf8 reply");
    assert!(text.ends_with(":nested"), "nested echo: {text}");
    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

#[tokio::test]
async fn same_thread_self_reentry_is_a_deterministic_error() {
    let ctx = Context::new();
    // svc-a backed by instance 42's DylibInvoke (never actually entered).
    let invoker = Invoker::new(Arc::new(DylibInvoke {
        exports: Exports {
            table: &raw const TARGET_TABLE,
            _lib: None,
        },
        handle: Handle(std::ptr::null_mut()),
        lock: InstanceLock::default(),
        instance_id: 42,
        alive: Arc::new(AtomicBool::new(true)),
    }));
    ctx.provide("svc-a", invoker).await.expect("provide");
    let raw = bare_activation(&ctx, &["svc-a"]);

    // Simulate executing inside instance 42 on this thread, then requiring
    // the service that resolves to instance 42: the entry check must reject
    // before the (non-reentrant) instance lock self-deadlocks.
    let entry = EntryGuard::enter(42);
    // SAFETY: `raw` names the live activation above.
    let (status, detail) = unsafe { raw_require(raw, b"svc-a", 0, b"") };
    assert_eq!(status, STATUS_REENTRANCY);
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "reentrancy_cycle");
    drop(entry);

    // SAFETY: unique reclaim; no further vtable calls.
    drop(unsafe { Arc::from_raw(raw.cast::<super::Activation>()) });
    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// E1.4: A -> mid -> A cycle dies as reentrancy, never as deadlock
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_cycle_through_intermediate_errors_not_deadlocks() {
    let ctx = Context::new();
    let world = boot_world(&ctx).await;
    // `call_framed` carries a 5s timeout: a regression to a deadlock fails
    // this test instead of hanging the suite.
    let (status, detail) = call_framed(&ctx, "svc-a", CONSUMER_REQUIRE_CYCLE, b"").await;
    assert_eq!(status, STATUS_REENTRANCY);
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "reentrancy_cycle");
    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// E1.6: emit three arms (delivery / invalid JSON / internal prefix)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_delivers_and_rejects_invalid_input() {
    let ctx = Context::new();
    let emitter: Arc<dyn Plugin> = Arc::new(from_static(&EMITTER_TABLE).expect("emitter table"));
    start(&ctx, emitter).await;
    let external_hits = count_listener(&ctx, "evt-x").await;

    // Arm 1: a well-formed emit reaches both the in-process listener and the
    // emitter's own dylib bridge listener (C1/C4).
    let (status, _) = call_framed(&ctx, "svc-emitter", EMITTER_EMIT, b"evt-x").await;
    assert_eq!(status, STATUS_OK);
    assert!(
        wait_until(|| external_hits.load(Ordering::SeqCst) >= 1).await,
        "in-process listener observed the event"
    );
    // The bridge-side count is read through the emitter's own HITS method.
    let bridge_hits = wait_until_bridge_hits(&ctx, 1).await;
    assert!(bridge_hits, "dylib bridge listener observed the event");

    // Arm 2: invalid JSON payload -> STATUS_INVALID_PAYLOAD (C2).
    let (status, _) = call_framed(&ctx, "svc-emitter", EMITTER_EMIT_BAD_JSON, b"evt-x").await;
    assert_eq!(status, STATUS_INVALID_PAYLOAD);

    // Arm 3: internal/* name -> STATUS_INVALID_EVENT (C3).
    let (status, _) = call_framed(&ctx, "svc-emitter", EMITTER_EMIT, b"internal/probe").await;
    assert_eq!(status, STATUS_INVALID_EVENT);

    ctx.stop().await;
}

async fn wait_until_bridge_hits(ctx: &Context, want: u32) -> bool {
    for _ in 0..200 {
        let (_, payload) = call_framed(ctx, "svc-emitter", EMITTER_HITS, b"").await;
        let hits: u32 = String::from_utf8(payload)
            .expect("utf8 hits")
            .parse()
            .expect("numeric hits");
        if hits >= want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

// ---------------------------------------------------------------------------
// E1.7: realm-scoped resolution (D15) through the vtable
// ---------------------------------------------------------------------------

/// In-realm probe: declares svc-a (its fiber's committed view resolves it
/// realm-locally), drives the consumer's `CALL_B`, and records the reply.
/// Realm services never leak outward (D15), so the probe must live inside.
fn realm_probe(sink: Arc<Mutex<Vec<Vec<u8>>>>) -> Arc<dyn Plugin> {
    dougong::plugin(
        "realm-probe",
        vec![Injection::new("svc-a")],
        move |ctx, _config| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                let reply = ctx
                    .get::<Invoker>("svc-a")
                    .expect("svc-a committed in the probe realm")
                    .call(CONSUMER_CALL_B, b"realm-probe".to_vec())
                    .await
                    .expect("probe call ok");
                let (_status, payload) = unpack(&reply);
                sink.lock().expect("sink").push(payload.to_vec());
                Ok(())
            })
        },
    )
}

/// The grant probe's sinked outcome: ABI status plus granter activation id.
type GrantOutcome = (i32, Option<u64>);

/// An in-realm probe that declares svc-a (so its committed view binds to the
/// realm-local provider), drives the consumer's `CONSUMER_GRANT`, and sinks
/// the ABI status plus the granter activation id it resolved.
fn grant_probe(sink: Arc<Mutex<Option<GrantOutcome>>>) -> Arc<dyn Plugin> {
    dougong::plugin(
        "grant-probe",
        vec![Injection::new("svc-a")],
        move |ctx, _config| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                let invoker = ctx
                    .get::<Invoker>("svc-a")
                    .expect("svc-a committed in the probe realm");
                let granter_id = invoker.inner.activation_id();
                let reply = invoker
                    .call(CONSUMER_GRANT, b"svc-b\0svc-a".to_vec())
                    .await
                    .expect("grant call ok");
                let (status, _) = unpack(&reply);
                sink.lock().expect("sink").replace((status, granter_id));
                Ok(())
            })
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_resolves_within_the_activation_realm() {
    let ctx = Context::new();
    // Global in-process providers shared by all realms via fallback.
    let host_svc = dougong::plugin("host-svc", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-host", Invoker::new(Arc::new(HostSvc)))
                .await?;
            Ok(())
        })
    });
    start(&ctx, host_svc).await;
    let mid = dougong::plugin("mid", vec![], |ctx, _config| {
        Box::pin(async move {
            let svc = MidSvc { ctx: ctx.clone() };
            ctx.provide("svc-mid", Invoker::new(Arc::new(svc))).await?;
            Ok(())
        })
    });
    start(&ctx, mid).await;

    let realm_a = ctx.isolate_realm(ctx.fresh_label());
    let realm_b = ctx.isolate_realm(ctx.fresh_label());

    // One target per realm: same table, distinct instance identities.
    let target_a: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target"));
    start(&realm_a, target_a).await;
    let target_b: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target"));
    start(&realm_b, target_b).await;

    let consumer_a: Arc<dyn Plugin> = Arc::new(from_static(&CONSUMER_TABLE).expect("consumer"));
    start(&realm_a, consumer_a).await;
    let consumer_b: Arc<dyn Plugin> = Arc::new(from_static(&CONSUMER_TABLE).expect("consumer"));
    start(&realm_b, consumer_b).await;

    // Drive each realm's consumer from inside the realm (realm services do
    // not leak outward by design, so an in-realm probe issues the call).
    let sink_a = Arc::new(Mutex::new(Vec::new()));
    start(&realm_a, realm_probe(Arc::clone(&sink_a))).await;
    let sink_b = Arc::new(Mutex::new(Vec::new()));
    start(&realm_b, realm_probe(Arc::clone(&sink_b))).await;

    let payload_a = {
        let sink = sink_a.lock().expect("sink");
        sink.first().expect("probe a ran").clone()
    };
    let payload_b = {
        let sink = sink_b.lock().expect("sink");
        sink.first().expect("probe b ran").clone()
    };
    // Same table, but each realm's consumer must have reached its own realm's
    // target instance (self-identifying replies carry the state address).
    assert_ne!(payload_a, payload_b, "realm-local binding (B3)");

    // Realm fallback: a third realm without a local target resolves the
    // global provider once one exists (started late on purpose). The reply
    // must embed the *global* target's identity, provable from the root.
    let global_target: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target"));
    start(&ctx, global_target).await;
    // Empty probe payload: the reply is exactly "target@<addr>:".
    let global_identity = call_raw(&ctx, "svc-b", 0, b"").await;

    let realm_c = ctx.isolate_realm(ctx.fresh_label());
    let consumer_c: Arc<dyn Plugin> = Arc::new(from_static(&CONSUMER_TABLE).expect("consumer"));
    start(&realm_c, consumer_c).await;
    let sink_c = Arc::new(Mutex::new(Vec::new()));
    start(&realm_c, realm_probe(Arc::clone(&sink_c))).await;
    let payload_c = {
        let sink = sink_c.lock().expect("sink");
        sink.first().expect("probe c ran").clone()
    };
    let identity_c = String::from_utf8(payload_c).expect("utf8 reply");
    let global_identity = String::from_utf8(global_identity).expect("utf8 identity");
    assert!(
        identity_c.starts_with(&global_identity),
        "realm c fell back to the global provider: {identity_c}"
    );

    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// E1.8: concurrent own-thread requires stay serialized per target instance
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_plugin_threads_serialize_on_the_target_instance() {
    let ctx = Context::new();
    let world = boot_world(&ctx).await;

    let (status, payload) = call_framed(&ctx, "svc-a", CONSUMER_SPAWN_HAMMER, b"").await;
    assert_eq!(status, STATUS_OK);
    let summary = String::from_utf8(payload).expect("utf8 summary");
    assert_eq!(summary, "0:9,0:9,0:9,0:9", "all four hammers got slow-done");

    let stats_raw = call_raw(&ctx, "svc-b", 2, b"").await;
    let stats_text = String::from_utf8(stats_raw).expect("utf8 stats");
    let max_seen = stats_text.split(',').nth(1).expect("max_seen field");
    assert_eq!(max_seen, "1", "target instance stayed serialized (A5/B7)");

    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// E1.9 + E1.10: reactivation rebuilds the vtable; setup-time require works
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reactivation_rebuilds_vtable_and_setup_require_worked() {
    let ctx = Context::new();
    let world = boot_world(&ctx).await;
    let teardowns = count_listener(&ctx, "consumer-teardown").await;

    // E1.10: the setup-time probe succeeded against the committed provider.
    // (SETUP_RESULT frames the probe's *status*; the payload is the target's
    // raw reply — services never frame.)
    let (probe_status, probe_payload) =
        call_framed(&ctx, "svc-a", CONSUMER_SETUP_RESULT, b"").await;
    assert_eq!(
        probe_status, STATUS_OK,
        "setup-time require succeeded (B5/G5)"
    );
    let probe_text = String::from_utf8(probe_payload).expect("utf8 probe");
    assert!(probe_text.ends_with(":setup-probe"));

    let (_, first_activation) = call_framed(&ctx, "svc-a", CONSUMER_ACTIVATION, b"").await;

    // Withdraw the provider: the consumer drains (teardown emits its event),
    // then parks.
    world.target.dispose().await;
    assert!(
        wait_until(|| teardowns.load(Ordering::SeqCst) == 1).await,
        "consumer teardown fired on withdrawal"
    );

    // A new provider instance re-activates the consumer: new setup, new
    // handle, new vtable (A3) — and the new probe targets the new instance.
    let target2: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target"));
    start(&ctx, target2).await;
    tokio::time::timeout(Duration::from_secs(5), world.consumer.join())
        .await
        .expect("consumer settles")
        .expect("consumer re-activated");

    let (_, second_activation) = call_framed(&ctx, "svc-a", CONSUMER_ACTIVATION, b"").await;
    assert_ne!(
        first_activation, second_activation,
        "new activation, new setup token"
    );
    let (probe_status, probe_payload) =
        call_framed(&ctx, "svc-a", CONSUMER_SETUP_RESULT, b"").await;
    assert_eq!(probe_status, STATUS_OK, "new vtable usable from new setup");
    let probe_text = String::from_utf8(probe_payload).expect("utf8 probe");
    assert!(probe_text.ends_with(":setup-probe"));
    assert_eq!(
        teardowns.load(Ordering::SeqCst),
        1,
        "the new activation has not torn down"
    );

    world.consumer.dispose().await;
    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// E1.11: teardown is serialized behind in-flight exports
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_waits_for_in_flight_calls() {
    let ctx = Context::new();
    let world = boot_world(&ctx).await;
    let consumer_teardowns = count_listener(&ctx, "consumer-teardown").await;
    let target_teardowns = count_listener(&ctx, "target-teardown").await;

    // A plugin-owned thread enters the target's slow (50ms) method.
    let slow = {
        let ctx = ctx.clone();
        tokio::spawn(
            async move { call_framed(&ctx, "svc-a", CONSUMER_REQUIRE_THREAD_SLOW, b"").await },
        )
    };
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Disposing the target drains the consumer first; the consumer teardown
    // cannot overlap its own in-flight export (it joins the require thread
    // through the consumer instance lock), and the target teardown cannot
    // overlap the slow call (target instance lock).
    world.target.dispose().await;
    let (status, payload) = slow.await.expect("slow task joined");
    assert_eq!(status, STATUS_OK);
    assert_eq!(payload, b"slow-done", "in-flight call completed cleanly");

    assert!(
        wait_until(|| consumer_teardowns.load(Ordering::SeqCst) == 1
            && target_teardowns.load(Ordering::SeqCst) == 1)
        .await,
        "both teardowns ran, after the in-flight call"
    );
    ctx.stop().await;
}

#[test]
fn dynplugin_declares_the_dylib_boundary_tag() {
    let plugin = from_static(&TARGET_TABLE).expect("adapter from static table");
    assert_eq!(Plugin::boundary_tag(&plugin), Some("dylib"));
}

// ---------------------------------------------------------------------------
// Cross-instance require cycle (D-V3-3 amendment): two dylib instances whose
// handlers require through an in-process relay. With both instance locks
// held, the second arming closes a wait cycle that must die as
// `reentrancy_cycle` instead of deadlocking either call.
// ---------------------------------------------------------------------------

/// Gated handler control: the handler announces entry, parks until the gate
/// opens, then issues its `require` through the copied vtable.
struct RingCtl {
    proceed: (Mutex<bool>, Condvar),
}

impl RingCtl {
    fn new() -> Self {
        Self {
            proceed: (Mutex::new(false), Condvar::new()),
        }
    }

    fn wait(&self) {
        let (lock, gate) = &self.proceed;
        drop(
            gate.wait_while(lock.lock().expect("gate lock"), |open| !*open)
                .expect("gate wait"),
        );
    }

    fn open(&self) {
        *self.proceed.0.lock().expect("gate lock") = true;
        self.proceed.1.notify_all();
    }
}

static RING1_CTL: Mutex<Option<Arc<RingCtl>>> = Mutex::new(None);
static RING2_CTL: Mutex<Option<Arc<RingCtl>>> = Mutex::new(None);
static RING1_HOST: Mutex<Option<SendHost>> = Mutex::new(None);
static RING2_HOST: Mutex<Option<SendHost>> = Mutex::new(None);
static RING1_ENTERED: AtomicBool = AtomicBool::new(false);
static RING2_ENTERED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn ring1_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"ring1", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn ring1_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"ring1","provide":["svc-ring1"],"inject":["svc-bridge"]}"#,
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

unsafe extern "C" fn ring2_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"ring2", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn ring2_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"ring2","provide":["svc-ring2"],"inject":["svc-bridge"]}"#,
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

unsafe extern "C" fn ring1_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    // SAFETY: the vtable contract hands `host` to `setup` exactly once; the
    // copy outlives the activation in the test's static slot.
    *RING1_HOST.lock().expect("ring1 host slot") = Some(SendHost(unsafe { *host }));
    Box::into_raw(Box::new(0_u8)).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn ring2_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    // SAFETY: as `ring1_setup`.
    *RING2_HOST.lock().expect("ring2 host slot") = Some(SendHost(unsafe { *host }));
    Box::into_raw(Box::new(0_u8)).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn ring_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the byte was allocated by the ring's `setup` for this
    // activation; teardown runs exactly once per successful setup.
    drop(unsafe { Box::from_raw(handle.cast::<u8>()) });
}

/// Gated ring handler: announce entry, wait for the gate, require through
/// the relay (`svc-bridge`), and frame the require outcome back. Reruns
/// (the unwinding cycle re-entering an unguarded instance) pass through the
/// same way, since each gate opens exactly once.
unsafe extern "C" fn ring1_call(
    _handle: *mut std::ffi::c_void,
    _method: u32,
    _data: *const u8,
    _len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    let host = RING1_HOST
        .lock()
        .expect("ring1 host slot")
        .as_ref()
        .expect("ring1 vtable copied at setup")
        .0;
    // Clone the handle and drop the slot guard: the guard must not stay
    // held across the gate wait or the require (the test's wait_until and
    // gate-open sections take the same ctl mutex).
    let ctl = Arc::clone(
        RING1_CTL
            .lock()
            .expect("ring1 ctl slot")
            .as_ref()
            .expect("ring1 ctl armed by the test"),
    );
    RING1_ENTERED.store(true, Ordering::SeqCst);
    ctl.wait();
    let (status, reply) = plugin_require(&host, b"svc-bridge", 0, b"ring1");
    pack(status, &reply, out, cap, out_len)
}

unsafe extern "C" fn ring2_call(
    _handle: *mut std::ffi::c_void,
    _method: u32,
    _data: *const u8,
    _len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    let host = RING2_HOST
        .lock()
        .expect("ring2 host slot")
        .as_ref()
        .expect("ring2 vtable copied at setup")
        .0;
    // As ring1_call: no guard stays held across the wait or the require.
    let ctl = Arc::clone(
        RING2_CTL
            .lock()
            .expect("ring2 ctl slot")
            .as_ref()
            .expect("ring2 ctl armed by the test"),
    );
    RING2_ENTERED.store(true, Ordering::SeqCst);
    ctl.wait();
    let (status, reply) = plugin_require(&host, b"svc-bridge", 1, b"ring2");
    pack(status, &reply, out, cap, out_len)
}

static RING1_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: ring1_name,
    descriptor: ring1_descriptor,
    setup: ring1_setup,
    call: ring1_call,
    on_event: consumer_on_event,
    on_next: consumer_on_next,
    teardown: ring_teardown,
};

static RING2_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: ring2_name,
    descriptor: ring2_descriptor,
    setup: ring2_setup,
    call: ring2_call,
    on_event: consumer_on_event,
    on_next: consumer_on_next,
    teardown: ring_teardown,
};

/// In-process relay: method 0 calls `svc-ring2`, method 1 calls `svc-ring1`.
struct BridgeSvc {
    ctx: Context,
}

impl dougong::Invoke for BridgeSvc {
    fn call(&self, method: u32, payload: Vec<u8>) -> dougong::BoxFuture<Result<Vec<u8>>> {
        let target = if method == 0 {
            "svc-ring2"
        } else {
            "svc-ring1"
        };
        let Some(invoker) = self.ctx.get::<Invoker>(target) else {
            return Box::pin(async move { Err(Error::msg(format!("{target} missing"))) });
        };
        invoker.call(0, payload)
    }
}

// The amendment's acceptance arm: ring1 and ring2 park inside their gated
// exports holding their instance locks; the requires through the relay then
// form the wait cycle ring1 -> ring2 -> ring1. The thread completing the
// cycle must receive a deterministic `reentrancy_cycle` (never a deadlock),
// and the wait graph must be edge-free once the calls unwind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_instance_require_cycle_dies_as_reentrancy() {
    let ctx = Context::new();
    // The relay first: both rings inject svc-bridge.
    let bridge = dougong::plugin("bridge", vec![], |ctx, _config| {
        Box::pin(async move {
            let svc = BridgeSvc { ctx: ctx.clone() };
            ctx.provide("svc-bridge", Invoker::new(Arc::new(svc)))
                .await?;
            Ok(())
        })
    });
    start(&ctx, bridge).await;
    *RING1_CTL.lock().expect("ring1 ctl slot") = Some(Arc::new(RingCtl::new()));
    let ring1: Arc<dyn Plugin> = Arc::new(from_static(&RING1_TABLE).expect("ring1 table"));
    start(&ctx, ring1).await;
    *RING2_CTL.lock().expect("ring2 ctl slot") = Some(Arc::new(RingCtl::new()));
    let ring2: Arc<dyn Plugin> = Arc::new(from_static(&RING2_TABLE).expect("ring2 table"));
    start(&ctx, ring2).await;

    let task1 = tokio::spawn({
        let ctx = ctx.clone();
        async move { call_framed(&ctx, "svc-ring1", 0, b"ping").await }
    });
    assert!(
        wait_until(|| RING1_ENTERED.load(Ordering::SeqCst)).await,
        "ring1 handler entered"
    );
    let task2 = tokio::spawn({
        let ctx = ctx.clone();
        async move { call_framed(&ctx, "svc-ring2", 0, b"ping").await }
    });
    assert!(
        wait_until(|| RING2_ENTERED.load(Ordering::SeqCst)).await,
        "ring2 handler entered"
    );

    // Both handlers hold their instance lock inside the gated export. Gate 1
    // sends ring1's require through the relay at ring2: it parks on ring2's
    // lock (still held by the gated ring2 handler). Gate 2 then sends
    // ring2's require at ring1 — arming that wait closes the cycle, which
    // must surface deterministically on this thread instead of parking.
    RING1_CTL
        .lock()
        .expect("ring1 ctl slot")
        .as_ref()
        .expect("ring1 ctl armed")
        .open();
    RING2_CTL
        .lock()
        .expect("ring2 ctl slot")
        .as_ref()
        .expect("ring2 ctl armed")
        .open();

    let (status1, payload1) = tokio::time::timeout(Duration::from_secs(5), task1)
        .await
        .expect("ring1 call settles (no deadlock)")
        .expect("ring1 task join");
    let (status2, payload2) = tokio::time::timeout(Duration::from_secs(5), task2)
        .await
        .expect("ring2 call settles (no deadlock)")
        .expect("ring2 task join");
    // Exactly one arming closes the cycle: that thread gets the direct
    // reentrancy status. The loser's parked leg unwinds through the relay,
    // which carries the inner framing as opaque bytes — its top-level
    // status stays OK with the nested detail inside the payload.
    let mut statuses = [status1, status2];
    statuses.sort_unstable();
    assert_eq!(statuses, [STATUS_REENTRANCY, STATUS_OK]);
    for (status, payload) in [(&status1, &payload1), (&status2, &payload2)] {
        let detail: Value = if *status == STATUS_REENTRANCY {
            serde_json::from_slice(payload).expect("framed detail")
        } else {
            serde_json::from_slice(&payload[4..]).expect("nested framed detail")
        };
        assert_eq!(detail["code"], "reentrancy_cycle");
    }
    // The unwound cycle leaves no wait edges behind.
    assert!(
        wait_until(super::wait_edges_empty).await,
        "wait edges must drain after the cycle unwinds"
    );
}

// ---------------------------------------------------------------------------
// ABI v4 E1 grant matrix (D31)
// ---------------------------------------------------------------------------

/// Serializes scenarios that intentionally share the global `svc-a` name.
/// Concurrency under test stays inside each scenario's own plugin threads.
static GRANT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct GrantLog {
    records: Arc<Mutex<Vec<LogRecord>>>,
}

impl GrantLog {
    fn grants(&self) -> Vec<LogRecord> {
        self.records
            .lock()
            .expect("grant log lock")
            .iter()
            .filter(|record| record.event == "outbound_grant")
            .cloned()
            .collect()
    }
}

impl Logger for GrantLog {
    fn log(&self, _level: LogLevel, _target: &str, _message: &str) {}

    fn log_record(&self, record: &LogRecord) {
        self.records
            .lock()
            .expect("grant log lock")
            .push(record.clone());
    }
}

/// Installs a structured logger without changing the default service labels.
async fn logged_context() -> (Context, GrantLog) {
    let root = Context::new();
    let ctx = root.isolate("logger");
    let sink = GrantLog::default();
    let logger: Arc<dyn Logger> = Arc::new(sink.clone());
    let plug = dougong::plugin("grant-test-logger", vec![], move |ctx, _config| {
        let logger = Arc::clone(&logger);
        Box::pin(async move {
            ctx.provide("logger", LoggerHandle::new(logger)).await?;
            Ok(())
        })
    });
    start(&ctx, plug).await;
    (ctx, sink)
}

// E1.1 (B1/C2): the callback round trip also crosses BoundaryInvoke.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_allows_callback_round_trip_through_boundary_decorator() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx = Context::new_with_options(ContextOptions {
        boundary_trace: true,
        ..ContextOptions::default()
    });
    let world = boot_world(&ctx).await;
    let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_GRANT, b"svc-b\0svc-a").await;
    assert_eq!(status, STATUS_OK);
    let (status, payload) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED, b"decision").await;
    assert_eq!(status, STATUS_OK);
    assert_eq!(payload, b"decision");
    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

// E1.4 (B2/B4): only descriptor-provided services can be granted.
#[tokio::test]
async fn grant_rejects_a_service_the_granter_does_not_own() {
    let ctx = Context::new();
    let host_svc = dougong::plugin("host-svc", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-host", Invoker::new(Arc::new(HostSvc)))
                .await?;
            Ok(())
        })
    });
    start(&ctx, host_svc).await;
    let raw = bare_activation_with_provide(&ctx, &[], &["svc-a"]);
    assert_eq!(
        // SAFETY: raw names the live detached activation.
        unsafe { raw_grant(raw, b"svc-host", b"svc-other") },
        STATUS_FAILED
    );
    // SAFETY: same live activation; direct inspection retains the detail code.
    let activation = unsafe { &*raw.cast::<super::Activation>() };
    let err = super::grant_impl(activation, b"svc-host", b"svc-other")
        .expect_err("not-owned grant must fail");
    assert_eq!(err.code, "grant_not_owned");
    assert_eq!(err.status, STATUS_FAILED);
    // SAFETY: unique reclaim after all vtable calls.
    unsafe { reclaim_activation(raw) };
    ctx.stop().await;
}

// E1.5 (B2/C3): target resolution is fail-closed at grant time.
#[tokio::test]
async fn grant_rejects_an_uncommitted_target() {
    let ctx = Context::new();
    let raw = bare_activation_with_provide(&ctx, &[], &["svc-a"]);
    assert_eq!(
        // SAFETY: raw names the live detached activation.
        unsafe { raw_grant(raw, b"missing", b"svc-a") },
        STATUS_INACTIVE
    );
    // SAFETY: same live activation.
    let activation = unsafe { &*raw.cast::<super::Activation>() };
    let err = super::grant_impl(activation, b"missing", b"svc-a")
        .expect_err("inactive-target grant must fail");
    assert_eq!(err.code, "inactive_dependency");
    assert_eq!(err.status, STATUS_INACTIVE);
    // SAFETY: unique reclaim after all vtable calls.
    unsafe { reclaim_activation(raw) };
    ctx.stop().await;
}

// E1.6 (B5/G3): teardown revokes and a new activation starts empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_dies_with_granter_and_does_not_resurrect() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx = Context::new();
    let world = boot_world(&ctx).await;
    let old_granter = ctx
        .get::<Invoker>("svc-a")
        .and_then(|invoker| invoker.inner.activation_id())
        .expect("old granter activation id");
    let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_GRANT, b"svc-b\0svc-a").await;
    assert_eq!(status, STATUS_OK);
    world.consumer.dispose().await;

    let (status, _) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED, b"after").await;
    assert_eq!(status, STATUS_UNDECLARED);
    assert!(
        super::grant_entries("svc-a")
            .into_iter()
            .all(|(granter, _)| granter != old_granter),
        "teardown removed every old-granter entry"
    );

    let consumer2: Arc<dyn Plugin> =
        Arc::new(from_static(&CONSUMER_TABLE).expect("new consumer table"));
    let consumer2 = start(&ctx, consumer2).await;
    let (status, _) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED, b"new").await;
    assert_eq!(
        status, STATUS_UNDECLARED,
        "the new activation did not inherit the old grant"
    );
    let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_GRANT, b"svc-b\0svc-a").await;
    assert_eq!(status, STATUS_OK);
    let (status, payload) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED, b"new").await;
    assert_eq!(status, STATUS_OK);
    assert_eq!(payload, b"new");
    consumer2.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

// E1.7 (B3/B6): duplicate registration is singular and every success audits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_is_idempotent_and_audited() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let (ctx, sink) = logged_context().await;
    let world = boot_world(&ctx).await;
    for _ in 0..2 {
        let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_GRANT, b"svc-b\0svc-a").await;
        assert_eq!(status, STATUS_OK);
    }
    let granter = ctx
        .get::<Invoker>("svc-a")
        .and_then(|invoker| invoker.inner.activation_id())
        .expect("granter activation id");
    assert_eq!(
        super::grant_entries("svc-a")
            .into_iter()
            .filter(|(candidate, _)| *candidate == granter)
            .count(),
        1,
        "duplicate grant stored one entry"
    );
    let successes: Vec<_> = sink
        .grants()
        .into_iter()
        .filter(|record| record.message.ends_with(" ok"))
        .collect();
    assert_eq!(successes.len(), 2, "each successful grant is audited");
    for record in successes {
        assert_eq!(record.level, LogLevel::Info);
        assert_eq!(record.service.as_deref(), Some("svc-a"));
        assert_eq!(record.boundary, Some("dylib"));
        assert!(record.trace.is_some());
        assert!(record.message.contains("target=svc-b"));
    }
    world.consumer.dispose().await;
    assert!(
        sink.grants()
            .iter()
            .any(|record| record.message == "cleared 1 grant(s)"),
        "batch cleanup emitted its summary"
    );
    world.target.dispose().await;
    ctx.stop().await;
}

// E1.8 (B1/G4/C3): neither another name nor another activation is authorized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_is_exactly_one_service_for_one_target_activation() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx = Context::new();
    let world = boot_world(&ctx).await;
    let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_GRANT, b"svc-b\0svc-a").await;
    assert_eq!(status, STATUS_OK);
    let (status, _) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_NAMED, b"svc-a-alt").await;
    assert_eq!(status, STATUS_UNDECLARED);

    let third_party = bare_activation(&ctx, &[]);
    // SAFETY: third_party names a live activation with no matching grant.
    let (status, detail) = unsafe { raw_require(third_party, b"svc-a", CONSUMER_ECHO, b"") };
    assert_eq!(status, STATUS_UNDECLARED);
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "undeclared_dependency");
    // SAFETY: unique reclaim after the require.
    unsafe { reclaim_activation(third_party) };
    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

// E1.13 (B1/C2): the authorization binds the provider resolved at require
// time. A realm-local granter's entry must not authorize the same-name
// provider the caller's own realm resolves — capability never transfers to
// an activation the granter did not grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_does_not_transfer_to_a_same_name_provider_in_another_realm() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx = Context::new();
    // The default realm holds the target (svc-b) and the decoy svc-a
    // provider (the world consumer, which never granted anything).
    let world = boot_world(&ctx).await;
    let realm = ctx.isolate_realm(ctx.fresh_label());
    let granter: Arc<dyn Plugin> = Arc::new(from_static(&CONSUMER_TABLE).expect("consumer"));
    let realm_granter = start(&realm, granter).await;
    // Realm services do not leak outward (D15), so an in-realm probe that
    // declares svc-a resolves the realm-local granter and drives its grant.
    let sink = Arc::new(Mutex::new(None));
    start(&realm, grant_probe(Arc::clone(&sink))).await;
    let (grant_status, realm_granter_id) = sink.lock().expect("grant sink").expect("probe ran");
    assert_eq!(grant_status, STATUS_OK);
    let realm_granter_id = realm_granter_id.expect("realm granter activation id");
    assert!(
        super::grant_entries("svc-a")
            .into_iter()
            .any(|(g, t)| g == realm_granter_id && t != 0),
        "the realm grant is recorded before the probe"
    );
    // The target's require resolves svc-a in the default realm (the decoy),
    // not the realm granter: the entry must not authorize it.
    let (status, detail) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED, b"x").await;
    assert_eq!(status, STATUS_UNDECLARED);
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "undeclared_dependency");
    assert_eq!(
        detail["message"],
        "cannot get property \"svc-a\" without inject"
    );
    realm_granter.dispose().await;
    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
    assert!(
        super::grant_entries("svc-a").is_empty(),
        "serial grant slot cleaned"
    );
}

// Review F2 (B5/C2, D31 amendment): a require that resolved the invoker
// before the provider's withdrawal must fail closed once teardown has run.
// Granted callers are not kernel dependency edges, so no drain joins them —
// the liveness gate under the instance lock is their only teardown ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_invoker_call_after_provider_teardown_fails_closed() {
    let ctx = Context::new();
    let target: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target table"));
    let target = start(&ctx, target).await;
    let invoker = ctx.get::<Invoker>("svc-b").expect("svc-b committed");
    target.dispose().await;
    let err = invoker
        .call(0, b"stale".to_vec())
        .await
        .expect_err("the liveness gate must fire");
    assert_eq!(err.code, "provider_gone");
    assert_eq!(err.class.as_str(), "transport");
    ctx.stop().await;
}

// E1.15 (C1/C3/G2, review F8): the grant name index is root-scoped. A grant
// made in one kernel root must not change another root's undeclared require
// classification — cross-root stays byte-identical UNDECLARED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_name_index_does_not_bleed_across_roots() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx_a = Context::new();
    let world = boot_world(&ctx_a).await;
    let (status, _) = call_framed(&ctx_a, "svc-a", CONSUMER_GRANT, b"svc-b\0svc-a").await;
    assert_eq!(status, STATUS_OK);
    assert!(
        !super::grant_entries("svc-a").is_empty(),
        "root A recorded the grant"
    );

    // Root B: a fresh kernel root in the same process. Its activation has no
    // inject and no grant; the legacy classification must hold even though
    // the process-global table holds root A's entry for the name.
    let ctx_b = Context::new();
    let raw = bare_activation(&ctx_b, &[]);
    // SAFETY: raw names a live activation in root B.
    let (status, detail) = unsafe { raw_require(raw, b"svc-a", CONSUMER_ECHO, b"") };
    assert_eq!(status, STATUS_UNDECLARED);
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "undeclared_dependency");
    assert_eq!(
        detail["message"],
        "cannot get property \"svc-a\" without inject"
    );
    // SAFETY: unique reclaim after the require.
    unsafe { reclaim_activation(raw) };
    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx_b.stop().await;
    ctx_a.stop().await;
    assert!(
        super::grant_entries("svc-a").is_empty(),
        "serial grant slot cleaned"
    );
}

// E1.16 (B2/B4, review F7): a target without an activation id (an
// in-process or wasm invoker) is rejected at grant time — a structurally
// inert capability must not be reported as success.
#[tokio::test]
async fn grant_rejects_a_target_without_an_activation_id() {
    let ctx = Context::new();
    let host_svc = dougong::plugin("host-svc", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-host", Invoker::new(Arc::new(HostSvc)))
                .await?;
            Ok(())
        })
    });
    start(&ctx, host_svc).await;
    let raw = bare_activation_with_provide(&ctx, &[], &["svc-inert"]);
    assert_eq!(
        // SAFETY: raw names the live detached activation.
        unsafe { raw_grant(raw, b"svc-host", b"svc-inert") },
        STATUS_FAILED
    );
    // SAFETY: same live activation; direct inspection retains the detail code.
    let activation = unsafe { &*raw.cast::<super::Activation>() };
    let err = super::grant_impl(activation, b"svc-host", b"svc-inert")
        .expect_err("in-process target grant must fail");
    assert_eq!(err.code, "grant_target_unsupported");
    assert_eq!(err.status, STATUS_FAILED);
    assert!(
        super::grant_entries("svc-inert").is_empty(),
        "no inert entry was stored"
    );
    // SAFETY: unique reclaim after all vtable calls.
    unsafe { reclaim_activation(raw) };
    ctx.stop().await;
}

// Review F9 (G2/C4): a provably empty grant table answers Missing through
// the atomic gate alone — a grant-free process keeps the legacy static-miss
// rejection lock-free. Holds the serialized slot so the table state is
// exclusive to this test.
#[tokio::test]
async fn empty_grant_table_takes_no_lock() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    assert!(
        !super::GRANTS_LIVE.load(Ordering::Relaxed),
        "the serialized slot starts with a drained table"
    );
    let locks = super::GRANT_LOCKS.load(Ordering::Relaxed);
    assert!(matches!(
        super::lookup_grant(1, 9, "svc-gate"),
        super::GrantLookup::Missing
    ));
    assert_eq!(
        super::GRANT_LOCKS.load(Ordering::Relaxed),
        locks,
        "the gate answered without the mutex"
    );
    // One committed entry flips the gate; the lookup now takes the lock and
    // reports the entry's granter for its target caller.
    super::GRANTS.lock().insert(
        "svc-gate".to_string(),
        vec![super::GrantEntry {
            granter: 7,
            root: 1,
            target_activation: 9,
        }],
    );
    super::GRANTS_LIVE.store(true, Ordering::Release);
    assert!(matches!(
        super::lookup_grant(1, 9, "svc-gate"),
        super::GrantLookup::Present { granters } if granters == [7]
    ));
    // Cross-root and wrong-caller probes still take the lock (the name is
    // live) and answer through the filtered path (F8/C1).
    assert!(matches!(
        super::lookup_grant(2, 9, "svc-gate"),
        super::GrantLookup::Missing
    ));
    assert!(matches!(
        super::lookup_grant(1, 10, "svc-gate"),
        super::GrantLookup::Present { granters } if granters.is_empty()
    ));
    assert_eq!(
        super::GRANT_LOCKS.load(Ordering::Relaxed),
        locks + 3,
        "each post-gate lookup took the mutex exactly once"
    );
    // Drain restores the gate for the next serialized test.
    super::GRANTS.lock().clear();
    super::GRANTS_LIVE.store(false, Ordering::Release);
}

// E1.9 (A3/C4): concurrent registration and callbacks are deterministic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_grants_and_requires_do_not_deadlock() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx = Context::new();
    let world = boot_world(&ctx).await;
    let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_GRANT, b"svc-b\0svc-a").await;
    assert_eq!(status, STATUS_OK);
    let grant_task = tokio::spawn({
        let ctx = ctx.clone();
        async move { call_framed(&ctx, "svc-a", CONSUMER_GRANT_HAMMER, b"").await }
    });
    let require_task = tokio::spawn({
        let ctx = ctx.clone();
        async move { call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED_HAMMER, b"").await }
    });
    let (grant_result, require_result) = tokio::time::timeout(Duration::from_secs(5), async {
        (grant_task.await, require_task.await)
    })
    .await
    .expect("concurrent grant/require did not deadlock");
    let (grant_status, grant_payload) = grant_result.expect("grant task joined");
    let (require_status, require_payload) = require_result.expect("require task joined");
    assert_eq!(
        (grant_status, grant_payload.as_slice()),
        (STATUS_OK, b"16".as_slice())
    );
    assert_eq!(
        (require_status, require_payload.as_slice()),
        (STATUS_OK, b"16".as_slice())
    );
    let granter = ctx
        .get::<Invoker>("svc-a")
        .and_then(|invoker| invoker.inner.activation_id())
        .expect("granter activation id");
    assert_eq!(
        super::grant_entries("svc-a")
            .into_iter()
            .filter(|(candidate, _)| *candidate == granter)
            .count(),
        1,
        "concurrent idempotent grants remained singular"
    );
    world.consumer.dispose().await;
    world.target.dispose().await;
    ctx.stop().await;
}

// E1.10 (B2/B4): both names are validated before ownership or resolution.
#[tokio::test]
async fn grant_rejects_non_utf8_names() {
    let ctx = Context::new();
    let raw = bare_activation_with_provide(&ctx, &[], &["svc-a"]);
    for (target, granted) in [
        (&[0xFF][..], b"svc-a".as_slice()),
        (b"svc-b".as_slice(), &[0xFE][..]),
    ] {
        // SAFETY: raw names a live activation and each slice is readable.
        assert_eq!(unsafe { raw_grant(raw, target, granted) }, STATUS_FAILED);
        // SAFETY: same live activation.
        let activation = unsafe { &*raw.cast::<super::Activation>() };
        let err = super::grant_impl(activation, target, granted)
            .expect_err("invalid grant name must fail");
        assert_eq!(err.code, "invalid_grant_name");
        assert_eq!(err.status, STATUS_FAILED);
    }
    // SAFETY: unique reclaim after all calls.
    unsafe { reclaim_activation(raw) };
    ctx.stop().await;
}

// E1.11 (G7/C1/C3): static hits bypass the table; name presence keeps INACTIVE.
#[tokio::test]
async fn require_grant_lookup_preserves_hot_path_and_two_level_priority() {
    // Writes the table (grant-missing), so it joins the serialized slot.
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx = Context::new();
    let host_svc = dougong::plugin("host-svc", vec![], |ctx, _config| {
        Box::pin(async move {
            ctx.provide("svc-host", Invoker::new(Arc::new(HostSvc)))
                .await?;
            Ok(())
        })
    });
    start(&ctx, host_svc).await;
    // The grant target must be a dylib activation (F7 rejects id-less
    // in-process invokers), so the name-index arm below boots one.
    let target: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target table"));
    let target = start(&ctx, target).await;
    let declared = bare_activation(&ctx, &["svc-host"]);
    let before = super::grant_lookups_on_this_thread();
    // SAFETY: declared names a live activation with a static capability.
    let (status, payload) = unsafe { raw_require(declared, b"svc-host", 0, b"hot") };
    assert_eq!(status, STATUS_OK);
    assert_eq!(payload, b"host:hot");
    assert_eq!(super::grant_lookups_on_this_thread(), before);
    // SAFETY: unique reclaim; no grants were issued by this activation.
    unsafe { reclaim_activation(declared) };

    let granter = bare_activation_with_provide(&ctx, &[], &["grant-missing"]);
    // SAFETY: granter is live; svc-b is a committed dylib target.
    let activation = unsafe { &*granter.cast::<super::Activation>() };
    super::grant_impl(activation, b"svc-b", b"grant-missing").expect("grant registers");
    let caller = bare_activation(&ctx, &[]);
    // SAFETY: caller is live, has no static declaration, and owns no grant.
    let (status, detail) = unsafe { raw_require(caller, b"grant-missing", 0, b"") };
    assert_eq!(status, STATUS_INACTIVE);
    let detail: Value = serde_json::from_slice(&detail).expect("detail is JSON");
    assert_eq!(detail["code"], "inactive_dependency");
    // SAFETY: unique reclaims; the caller is reclaimed before the granter so
    // the grant-name entry remains present for the assertion above.
    unsafe { reclaim_activation(caller) };
    // SAFETY: the granter has its own unique reclaim after all grant checks.
    unsafe { reclaim_activation(granter) };
    target.dispose().await;
    ctx.stop().await;
}

// E1.12 (B1 reason 2): target replacement drives a fresh grant and binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_reactivation_rebinds_grant_through_granter_reactivation() {
    let _serial = GRANT_TEST_LOCK.lock().await;
    let ctx = Context::new();
    let target1: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target one"));
    let target1 = start(&ctx, target1).await;
    let consumer: Arc<dyn Plugin> =
        Arc::new(from_static(&GRANT_CONSUMER_TABLE).expect("grant consumer"));
    let consumer = start(&ctx, consumer).await;
    let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_SETUP_GRANT, b"").await;
    assert_eq!(status, STATUS_OK);
    let first_target = ctx
        .get::<Invoker>("svc-b")
        .and_then(|invoker| invoker.inner.activation_id())
        .expect("first target id");
    let first_granter = ctx
        .get::<Invoker>("svc-a")
        .and_then(|invoker| invoker.inner.activation_id())
        .expect("first granter id");
    assert!(super::grant_entries("svc-a").contains(&(first_granter, first_target)));
    let (status, payload) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED, b"first").await;
    assert_eq!(
        (status, payload.as_slice()),
        (STATUS_OK, b"first".as_slice())
    );

    target1.dispose().await;
    assert!(
        super::grant_entries("svc-a")
            .into_iter()
            .all(|(granter, _)| granter != first_granter),
        "the drained granter activation revoked its old binding"
    );
    let target2: Arc<dyn Plugin> = Arc::new(from_static(&TARGET_TABLE).expect("target two"));
    let target2 = start(&ctx, target2).await;
    tokio::time::timeout(Duration::from_secs(5), consumer.join())
        .await
        .expect("consumer reactivation settles")
        .expect("consumer reactivation succeeds");
    let second_target = ctx
        .get::<Invoker>("svc-b")
        .and_then(|invoker| invoker.inner.activation_id())
        .expect("second target id");
    let second_granter = ctx
        .get::<Invoker>("svc-a")
        .and_then(|invoker| invoker.inner.activation_id())
        .expect("second granter id");
    assert_ne!(first_target, second_target);
    assert_ne!(first_granter, second_granter);
    assert!(super::grant_entries("svc-a").contains(&(second_granter, second_target)));
    assert!(
        !super::grant_entries("svc-a").contains(&(second_granter, first_target)),
        "the new grant is bound to the new target, not the old identity"
    );
    let (status, _) = call_framed(&ctx, "svc-a", CONSUMER_SETUP_GRANT, b"").await;
    assert_eq!(status, STATUS_OK);
    let (status, payload) = call_framed(&ctx, "svc-b", TARGET_REQUIRE_GRANTED, b"second").await;
    assert_eq!(
        (status, payload.as_slice()),
        (STATUS_OK, b"second".as_slice())
    );
    consumer.dispose().await;
    target2.dispose().await;
    ctx.stop().await;
}
