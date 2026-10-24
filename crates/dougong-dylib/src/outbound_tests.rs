// Rust guideline compliant 2026-10-24
//! E1 test matrix for the ABI v3 outbound surface (require/emit vtable).
//!
//! The fixtures are hand-written "real" plugins over static export tables:
//! they copy the `HostExports` vtable at `setup` and call back through it
//! from handler threads and from plugin-owned threads, exactly as a
//! third-party `cdylib` would. Framing between test and plugin is
//! `4-byte LE status || payload` so the test can assert host status codes.
//!
//! Every test builds its own `Context` root, so parallel test threads share
//! nothing but the stateless static tables.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, Error, FiberHandle, Injection, Invoker, Plugin, Result};
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
const CONSUMER_STATE_ADDR: u32 = 6;
const CONSUMER_SPAWN_HAMMER: u32 = 8;

struct ConsumerState {
    host: HostExports,
    /// Result of the require issued *during* `setup` (E1.10 probe).
    setup_probe: (i32, Vec<u8>),
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
            br#"{"name":"fake-consumer","provide":["svc-a"],"inject":["svc-b","svc-mid","svc-host"]}"#,
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
    Box::into_raw(Box::new(ConsumerState { host, setup_probe })).cast::<std::ffi::c_void>()
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
        CONSUMER_STATE_ADDR => {
            let addr = format!("{state:p}");
            pack(0, addr.as_bytes(), out, cap, out_len)
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

// ---------------------------------------------------------------------------
// Target fixture: provides svc-b (echo / slow / stats)
// ---------------------------------------------------------------------------

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
    Arc::into_raw(Arc::new(super::Activation {
        ctx: ctx.clone(),
        rt: tokio::runtime::Handle::current(),
        inject: inject.iter().map(|name| (*name).to_string()).collect(),
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

    let (_, first_addr) = call_framed(&ctx, "svc-a", CONSUMER_STATE_ADDR, b"").await;

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

    let (_, second_addr) = call_framed(&ctx, "svc-a", CONSUMER_STATE_ADDR, b"").await;
    assert_ne!(first_addr, second_addr, "new activation, new handle");
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
