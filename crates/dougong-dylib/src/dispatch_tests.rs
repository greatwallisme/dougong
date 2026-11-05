//! Outbound `dispatch` tests (ABI v5, D32 / `dispatch_requirements.md` §3 E1).
//!
//! Fixtures mirror `outbound_tests`: static export tables embedded in the
//! test binary; the driver fixture reaches `host_dispatch` from plugin-owned
//! threads (the B6 posture), the listener fixture exercises the CPS bridge
//! (transform / veto / defer / fail). Background dispatches are keyed by
//! test-supplied nonces in one shared registry, so concurrent tests never
//! observe each other's results.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, Error, Invoker, Listener, Plugin};
use dougong_abi::{
    ABI_VERSION, HostExports, MODE_BAIL, MODE_EMIT, MODE_PARALLEL, MODE_SERIAL, MODE_WATERFALL,
    PluginExports, STATUS_CAPACITY, STATUS_DEFERRED, STATUS_FAILED, STATUS_INVALID_EVENT,
    STATUS_INVALID_PAYLOAD, STATUS_OK, STATUS_REENTRANCY, write_out,
};
use serde_json::{Value, json};

use super::arm_deadlock_watchdog;
use super::from_static;

// ---------------------------------------------------------------------------
// Framing helpers (test-side convention: `status || payload`)
// ---------------------------------------------------------------------------

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

fn unpack(bytes: &[u8]) -> (i32, &[u8]) {
    let (head, payload) = bytes.split_at(4);
    (
        i32::from_le_bytes(head.try_into().expect("framing prefix")),
        payload,
    )
}

/// `Send` wrapper for a copied vtable: `dispatch` is contractually callable
/// from any plugin-owned thread (D32 A3), and the fixtures hand the copy to
/// spawned threads only.
struct SendHost(HostExports);
// SAFETY: the vtable contract states the entries are callable from any
// plugin-owned thread concurrently; the host implementations are `Send + Sync`.
unsafe impl Send for SendHost {}

/// Plugin-side growable `dispatch`: retries with a doubled buffer whenever
/// the host asks for more room (the caller-owned buffer protocol).
fn plugin_dispatch(host: &SendHost, event: &[u8], mode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
    let mut cap = 4096_usize;
    loop {
        let mut buf = vec![0_u8; cap];
        let mut len = 0_usize;
        // SAFETY: the vtable is live (fixtures never outlive their
        // activation); buffers are caller-owned.
        let status = unsafe {
            (host.0.dispatch)(
                host.0.host_handle,
                event.as_ptr(),
                event.len(),
                mode,
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

// ---------------------------------------------------------------------------
// Driver fixture: provides svc-dispatcher (reaches host_dispatch for tests)
// ---------------------------------------------------------------------------

const DRIVER_DISPATCH_JOIN: u32 = 0;
const DRIVER_DISPATCH_IN_ENTRY: u32 = 1;
const DRIVER_DISPATCH_BG: u32 = 2;
const DRIVER_DISPATCH_POLL: u32 = 3;
const DRIVER_PING: u32 = 4;

struct DriverState {
    host: SendHost,
}

unsafe extern "C" fn driver_name(out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"dispatch-driver", out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn driver_descriptor(out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"dispatch-driver","provide":["svc-dispatcher"],"inject":[],"listen":{}}"#,
            out,
            cap,
            out_len,
        )
    } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn driver_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of setup; copied by value (v3 contract).
    let host = unsafe { *host };
    Box::into_raw(Box::new(DriverState {
        host: SendHost(host),
    }))
    .cast::<std::ffi::c_void>()
}

/// Background dispatch results keyed by test-supplied nonces (race-free:
/// one global mutex, unique nonces per dispatch).
static BG_RESULTS: Mutex<Vec<(u64, i32, Vec<u8>)>> = Mutex::new(Vec::new());

fn envelope_parts(payload: &[u8]) -> (String, u32, Vec<u8>, u64) {
    let envelope: Value = serde_json::from_slice(payload).expect("test envelope parses");
    let event = envelope["event"].as_str().expect("event").to_string();
    let mode = u32::try_from(envelope["mode"].as_u64().expect("mode")).expect("mode fits u32");
    let body = match &envelope["body"] {
        Value::String(raw) => raw.clone().into_bytes(),
        other => serde_json::to_vec(other).expect("body serializes"),
    };
    let nonce = envelope["nonce"].as_u64().unwrap_or(0);
    (event, mode, body, nonce)
}

unsafe extern "C" fn driver_call(
    handle: *mut std::ffi::c_void,
    method: u32,
    payload: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if handle.is_null() {
        return STATUS_FAILED;
    }
    // SAFETY: the handle came from `driver_setup` and the per-instance lock
    // serializes every foreign entry touching it.
    let state = unsafe { &*(handle.cast::<DriverState>()) };
    // SAFETY: vtable input contract (the payload slice is call-scoped).
    let bytes = unsafe { std::slice::from_raw_parts(payload, len) };
    match method {
        DRIVER_DISPATCH_JOIN => {
            let (event, mode, body, _) = envelope_parts(bytes);
            let host = SendHost(state.host.0);
            let joined =
                std::thread::spawn(move || plugin_dispatch(&host, event.as_bytes(), mode, &body))
                    .join()
                    .expect("dispatch thread");
            pack(joined.0, &joined.1, out, cap, out_len)
        }
        DRIVER_DISPATCH_IN_ENTRY => {
            // Direct call on the foreign-entry thread: the B6 posture gate
            // must reject it deterministically (E1.8).
            let (event, mode, body, _) = envelope_parts(bytes);
            let outcome = plugin_dispatch(&state.host, event.as_bytes(), mode, &body);
            pack(outcome.0, &outcome.1, out, cap, out_len)
        }
        DRIVER_DISPATCH_BG => {
            let (event, mode, body, nonce) = envelope_parts(bytes);
            let host = SendHost(state.host.0);
            std::thread::spawn(move || {
                let outcome = plugin_dispatch(&host, event.as_bytes(), mode, &body);
                BG_RESULTS
                    .lock()
                    .expect("bg registry")
                    .push((nonce, outcome.0, outcome.1));
            });
            pack(STATUS_OK, b"bg", out, cap, out_len)
        }
        DRIVER_DISPATCH_POLL => {
            let nonce = envelope_parts(bytes).3;
            // One lock scope spans position + removal so a concurrent poll
            // cannot observe a stale index.
            let found = {
                let mut results = BG_RESULTS.lock().expect("bg registry");
                results
                    .iter()
                    .position(|entry| entry.0 == nonce)
                    .map(|pos| results.remove(pos))
            };
            found.map_or_else(
                || pack(STATUS_OK, b"pending", out, cap, out_len),
                |(_, status, reply)| pack(status, &reply, out, cap, out_len),
            )
        }
        DRIVER_PING => pack(STATUS_OK, b"dispatcher-ping", out, cap, out_len),
        _ => STATUS_FAILED,
    }
}

unsafe extern "C" fn driver_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the handle came from `driver_setup` and is torn down once.
    drop(unsafe { Box::from_raw(handle.cast::<DriverState>()) });
}

unsafe extern "C" fn driver_on_event(
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

unsafe extern "C" fn driver_on_next(
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

static DRIVER_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: driver_name,
    descriptor: driver_descriptor,
    setup: driver_setup,
    call: driver_call,
    on_event: driver_on_event,
    on_next: driver_on_next,
    teardown: driver_teardown,
};

// ---------------------------------------------------------------------------
// Listener fixture: CPS arms keyed by listened event id
// ---------------------------------------------------------------------------

const EVT_ECHO: u32 = 1;
const EVT_VETO: u32 = 2;
#[expect(
    dead_code,
    reason = "evt-fail maps to the wildcard STATUS_FAILED arm; the id stays pinned next to its descriptor entry"
)]
const EVT_FAIL: u32 = 4;
const EVT_ORDER: u32 = 5;
const EVT_SERIAL: u32 = 6;
const EVT_REQUIRE_LOOP: u32 = 7;

static ON_NEXT_HITS: AtomicU64 = AtomicU64::new(0);

struct ListenerState {
    host: SendHost,
}

unsafe extern "C" fn listener_name(out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"dispatch-listener", out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn listener_descriptor(out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"dispatch-listener","provide":["svc-listener"],"inject":["svc-dispatcher"],"listen":{"evt-echo":1,"evt-veto":2,"evt-fail":4,"evt-order":5,"evt-serial":6,"evt-req":7}}"#,
            out,
            cap,
            out_len,
        )
    } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn listener_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of setup; copied by value.
    let host = unsafe { *host };
    Box::into_raw(Box::new(ListenerState {
        host: SendHost(host),
    }))
    .cast::<std::ffi::c_void>()
}

unsafe extern "C" fn listener_on_event(
    handle: *mut std::ffi::c_void,
    event: u32,
    _mode: u32,
    data: *const u8,
    len: usize,
    _next: u32,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: the handle came from `listener_setup`; the per-instance lock
    // serializes foreign entries, and the host vtable copy is read-only.
    let state = unsafe { &*(handle.cast::<ListenerState>()) };
    // SAFETY: vtable input contract (call-scoped payload slice).
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    match event {
        EVT_ECHO | EVT_ORDER => {
            let mut value: Value = serde_json::from_slice(bytes).expect("chain payload parses");
            let tag = if event == EVT_ECHO { "seen" } else { "dylib" };
            if let Value::Object(ref mut map) = value {
                map.insert(tag.to_string(), Value::Bool(true));
            }
            let transformed = serde_json::to_vec(&value).expect("transform serializes");
            // Event outputs ride the kernel bridge raw — no test framing.
            write_raw(&transformed, out, cap, out_len);
            STATUS_DEFERRED
        }
        EVT_VETO => {
            write_raw(br#"{"vetoed":true}"#, out, cap, out_len);
            STATUS_OK
        }
        EVT_SERIAL => {
            write_raw(br#"{"serial-bail":true}"#, out, cap, out_len);
            STATUS_OK
        }
        EVT_REQUIRE_LOOP => {
            // E1.9 positive arm: a chain listener requires the dispatcher's
            // own committed service; the dispatcher's instance lock is free
            // because the dispatch came from its plugin-owned background
            // thread (B6).
            let host = SendHost(state.host.0);
            let (status, reply) = std::thread::spawn(move || {
                plugin_require(&host, b"svc-dispatcher", DRIVER_PING, b"ping")
            })
            .join()
            .expect("require thread");
            if status != STATUS_OK {
                return STATUS_FAILED;
            }
            // The driver replies in test framing (`status || payload`).
            let (_, payload) = unpack(&reply);
            let ping = json!({ "ping": String::from_utf8_lossy(payload).to_string() });
            let transformed = serde_json::to_vec(&ping).expect("transform serializes");
            write_raw(&transformed, out, cap, out_len);
            STATUS_DEFERRED
        }
        // EVT_FAIL lands here: a bare `STATUS_FAILED` reply is the contract
        // for a failing listener, same as any unknown event id.
        _ => STATUS_FAILED,
    }
}

/// Writes a raw JSON payload into the caller-owned output buffer (the
/// event-output contract; unlike `call` replies, bridge outputs carry no
/// test framing).
fn write_raw(payload: &[u8], out: *mut u8, cap: usize, out_len: *mut usize) {
    // SAFETY: host buffer contract (caller-owned out buffer).
    if !unsafe { write_out(payload, out, cap, out_len) } {
        // A capacity miss surfaces as a zero-length reply; the bridge then
        // retries the whole export with a larger buffer.
        // SAFETY: the length pointer is part of the same contract.
        unsafe {
            if !out_len.is_null() {
                *out_len = 0;
            }
        }
    }
}

/// Plugin-side growable `require` (the v2 caller-owned buffer protocol).
fn plugin_require(host: &SendHost, service: &[u8], method: u32, payload: &[u8]) -> (i32, Vec<u8>) {
    let mut cap = 4096_usize;
    loop {
        let mut buf = vec![0_u8; cap];
        let mut len = 0_usize;
        // SAFETY: the vtable is live (fixtures never outlive their
        // activation); buffers are caller-owned.
        let status = unsafe {
            (host.0.require)(
                host.0.host_handle,
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

unsafe extern "C" fn listener_on_next(
    _handle: *mut std::ffi::c_void,
    _event: u32,
    status: i32,
    data: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    ON_NEXT_HITS.fetch_add(1, Ordering::SeqCst);
    if status != STATUS_OK {
        return STATUS_FAILED;
    }
    // SAFETY: vtable input contract.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    // Continuation outputs ride the bridge raw (no test framing).
    write_raw(bytes, out, cap, out_len);
    STATUS_OK
}

unsafe extern "C" fn listener_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the handle came from `listener_setup` and is torn down once.
    drop(unsafe { Box::from_raw(handle.cast::<ListenerState>()) });
}

unsafe extern "C" fn listener_call(
    _handle: *mut std::ffi::c_void,
    _method: u32,
    _payload: *const u8,
    _len: usize,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    STATUS_FAILED
}

static LISTENER_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: listener_name,
    descriptor: listener_descriptor,
    setup: listener_setup,
    call: listener_call,
    on_event: listener_on_event,
    on_next: listener_on_next,
    teardown: listener_teardown,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn start(ctx: &Context, plugin: Arc<dyn Plugin>) {
    arm_deadlock_watchdog();
    let handle = ctx.plugin(plugin, None).expect("spawn");
    tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("activation settles")
        .expect("activation ok");
}

/// Starts the bridged listener and the driver it injects, then waits until
/// the listener has converged. The listener declares `inject: [svc-dispatcher]`,
/// so its first `join` returns while it is still `Pending` (nothing is busy
/// until the driver provides the service); only a second `join` after the
/// driver is up observes the activation that registers the event bridges.
/// Dispatching before that point races the registration.
async fn start_pair(ctx: &Context) {
    arm_deadlock_watchdog();
    let listener: Arc<dyn Plugin> = Arc::new(from_static(&LISTENER_TABLE).expect("listener table"));
    let listener = ctx.plugin(listener, None).expect("spawn listener");
    let driver: Arc<dyn Plugin> = Arc::new(from_static(&DRIVER_TABLE).expect("driver table"));
    start(ctx, driver).await;
    tokio::time::timeout(Duration::from_secs(5), listener.join())
        .await
        .expect("listener settles")
        .expect("listener ok");
    assert_eq!(
        listener.state(),
        dougong::FiberState::Active,
        "the listener converged once its dependency arrived"
    );
}

/// Boots listener then driver and returns the context.
async fn boot() -> Context {
    let ctx = Context::new();
    start_pair(&ctx).await;
    ctx
}

/// Boots with one in-process listener registered before the bridged one, so
/// a serial/bail chain meets the in-process arm first (chain order =
/// registration order); the bridged listener's non-null output then bails.
async fn boot_with_in_process_listener(name: &str, result: Value) -> Context {
    let ctx = Context::new();
    in_process_listener(&ctx, name, result).await;
    start_pair(&ctx).await;
    ctx
}

/// Calls a driver method through the dynamic channel with a deadlock
/// guardrail, then unpacks the test framing.
async fn call_driver(ctx: &Context, method: u32, payload: &[u8]) -> (i32, Vec<u8>) {
    let invoker = ctx
        .get::<Invoker>("svc-dispatcher")
        .expect("driver committed");
    let bytes = tokio::time::timeout(
        Duration::from_secs(30),
        invoker.call(method, payload.to_vec()),
    )
    .await
    .expect("no deadlock")
    .expect("call ok");
    let (status, reply) = unpack(&bytes);
    (status, reply.to_vec())
}

/// Runs one joined dispatch through the driver's plugin-owned thread.
async fn dispatch_join(ctx: &Context, event: &str, mode: u32, body: Value) -> (i32, Vec<u8>) {
    let envelope = json!({ "event": event, "mode": mode, "body": body });
    call_driver(ctx, DRIVER_DISPATCH_JOIN, envelope.to_string().as_bytes()).await
}

/// Runs one background dispatch and polls its nonce-keyed result.
async fn dispatch_bg(
    ctx: &Context,
    nonce: u64,
    event: &str,
    mode: u32,
    body: Value,
) -> (i32, Vec<u8>) {
    let envelope = json!({ "event": event, "mode": mode, "body": body, "nonce": nonce });
    call_driver(ctx, DRIVER_DISPATCH_BG, envelope.to_string().as_bytes()).await;
    for _ in 0..600 {
        let poll = json!({ "event": "poll", "mode": 0, "body": 0, "nonce": nonce });
        let (status, reply) =
            call_driver(ctx, DRIVER_DISPATCH_POLL, poll.to_string().as_bytes()).await;
        if status != STATUS_OK {
            return (status, reply);
        }
        if reply != b"pending" {
            return (status, reply);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("background dispatch {nonce} never settled");
}

/// Registers one in-process listener returning `result` (a `Value::Null`
/// reply continues a serial chain; anything else bails).
async fn in_process_listener(ctx: &Context, name: &str, result: Value) {
    let listener: Listener = Arc::new(move |_ctx, _payload, _next| {
        let result = result.clone();
        Box::pin(async move { Ok(result) })
    });
    ctx.on(name, listener).await.expect("listener registers");
}

// ---------------------------------------------------------------------------
// E1 matrix (dispatch_requirements.md §3)
// ---------------------------------------------------------------------------

/// E1.1 + E1.4: basic roundtrip through a CPS listener — the dylib listener
/// defers with a transform, the host pulls the continuation, `on_next`
/// delivers the chain value, and the dispatcher receives the merged payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_roundtrip_through_deferred_listener() {
    let ctx = boot().await;
    ON_NEXT_HITS.store(0, Ordering::SeqCst);
    let (status, reply) = dispatch_join(&ctx, "evt-echo", MODE_WATERFALL, json!({ "n": 1 })).await;
    assert_eq!(
        status,
        STATUS_OK,
        "reply: {}",
        String::from_utf8_lossy(&reply)
    );
    let value: Value = serde_json::from_slice(&reply).expect("reply parses");
    assert_eq!(value, json!({ "n": 1, "seen": true }));
    assert!(ON_NEXT_HITS.load(Ordering::SeqCst) >= 1, "continuation ran");
}

/// E1.2: with no listener registered, the waterfall's identity continuation
/// reports the original payload bytes verbatim (B4 byte stability).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untouched_chain_echoes_original_payload_bytes() {
    let ctx = boot().await;
    let envelope =
        json!({ "event": "evt-nobody", "mode": MODE_WATERFALL, "body": "{\"k\":\"v\"}" });
    let (status, reply) =
        call_driver(&ctx, DRIVER_DISPATCH_JOIN, envelope.to_string().as_bytes()).await;
    assert_eq!(status, STATUS_OK);
    assert_eq!(reply, b"{\"k\":\"v\"}");
}

/// E1.3: a listener that answers without pulling the continuation vetoes the
/// chain; its output is the final value the dispatcher receives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_output_without_next_vetoes_chain() {
    let ctx = boot().await;
    let (status, reply) = dispatch_join(&ctx, "evt-veto", MODE_WATERFALL, json!({ "n": 1 })).await;
    assert_eq!(status, STATUS_OK);
    let value: Value = serde_json::from_slice(&reply).expect("reply parses");
    assert_eq!(value, json!({ "vetoed": true }));
}

/// E1.5: in-process and bridged listeners compose one chain in registration
/// order — the array tag proves the in-process listener ran first and the
/// bridged listener transformed after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_chain_runs_in_registration_order() {
    let ctx = Context::new();
    let listener: Listener = Arc::new(|_ctx, payload, next| {
        Box::pin(async move {
            let mut value = payload;
            if let Value::Object(ref mut map) = value {
                map.insert("order".to_string(), json!(["in-process"]));
            }
            next.call(value).await
        })
    });
    ctx.on("evt-order", listener)
        .await
        .expect("listener registers");
    start_pair(&ctx).await;
    let (status, reply) = dispatch_join(&ctx, "evt-order", MODE_WATERFALL, json!({})).await;
    assert_eq!(status, STATUS_OK);
    let value: Value = serde_json::from_slice(&reply).expect("reply parses");
    assert_eq!(
        value,
        json!({ "order": ["in-process"], "dylib": true }),
        "in-process listener runs first, bridged listener transforms after"
    );
}

/// E1.6: a failing chain listener surfaces as request-visible
/// `STATUS_FAILED` with the `{"code","class","message"}` detail triple (B3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listener_failure_is_request_visible() {
    let ctx = boot().await;
    let (status, reply) = dispatch_join(&ctx, "evt-fail", MODE_WATERFALL, json!({})).await;
    assert_eq!(status, STATUS_FAILED);
    let detail: Value = serde_json::from_slice(&reply).expect("detail parses");
    assert_eq!(detail["code"], "listener_failed");
    assert_eq!(detail["class"], "logic");
    assert_ne!(detail["message"].as_str().expect("message"), "");
}

/// E1.7: deterministic input validation — non-JSON payload, `internal/*`
/// event names, and out-of-vocabulary modes all fail with fixed statuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_dispatch_inputs_fail_deterministically() {
    let ctx = boot().await;
    // Non-JSON payload (raw bytes ride the envelope's string body).
    let envelope = json!({ "event": "evt-echo", "mode": MODE_WATERFALL, "body": "not-json{{" });
    let (status, reply) =
        call_driver(&ctx, DRIVER_DISPATCH_JOIN, envelope.to_string().as_bytes()).await;
    assert_eq!(status, STATUS_INVALID_PAYLOAD);
    assert_eq!(reply, b"");
    // internal/* event name.
    let envelope = json!({ "event": "internal/x", "mode": MODE_WATERFALL, "body": {} });
    let (status, reply) =
        call_driver(&ctx, DRIVER_DISPATCH_JOIN, envelope.to_string().as_bytes()).await;
    assert_eq!(status, STATUS_INVALID_EVENT);
    assert_eq!(reply, b"");
    // Out-of-vocabulary mode.
    let (status, reply) = dispatch_join(&ctx, "evt-echo", 99, json!({})).await;
    assert_eq!(status, STATUS_FAILED);
    let detail: Value = serde_json::from_slice(&reply).expect("detail parses");
    assert_eq!(detail["code"], "invalid_dispatch_mode");
}

/// E1.8: dispatch from inside a foreign entry of the driver is rejected
/// deterministically with `STATUS_REENTRANCY` (B6 posture gate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_inside_foreign_entry_is_rejected() {
    let ctx = boot().await;
    let envelope = json!({ "event": "evt-echo", "mode": MODE_WATERFALL, "body": {} });
    let (status, reply) = call_driver(
        &ctx,
        DRIVER_DISPATCH_IN_ENTRY,
        envelope.to_string().as_bytes(),
    )
    .await;
    assert_eq!(status, STATUS_REENTRANCY);
    let detail: Value = serde_json::from_slice(&reply).expect("detail parses");
    assert_eq!(detail["code"], "reentrancy_cycle");
}

/// E1.9: a chain listener requiring the dispatcher's own committed service
/// proceeds normally when the dispatch originates on a plugin-owned
/// background thread (B6 positive arm) — no deadlock, correct reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chain_listener_reaches_dispatcher_service_without_deadlock() {
    let ctx = boot().await;
    let (status, reply) = dispatch_bg(&ctx, 0xE901, "evt-req", MODE_WATERFALL, json!({})).await;
    assert_eq!(status, STATUS_OK);
    let value: Value = serde_json::from_slice(&reply).expect("reply parses");
    assert_eq!(value, json!({ "ping": "dispatcher-ping" }));
}

/// E1.10: export tables of earlier ABI revisions (v4 and the immediately
/// preceding revision) are rejected by the current host with both versions
/// named (D1 version gate).
#[test]
fn earlier_export_tables_are_rejected_by_the_current_host() {
    static V4_TABLE: PluginExports = PluginExports {
        abi_version: 4,
        name: driver_name,
        descriptor: driver_descriptor,
        setup: driver_setup,
        call: driver_call,
        on_event: driver_on_event,
        on_next: driver_on_next,
        teardown: driver_teardown,
    };
    static PREVIOUS_TABLE: PluginExports = PluginExports {
        abi_version: ABI_VERSION - 1,
        name: driver_name,
        descriptor: driver_descriptor,
        setup: driver_setup,
        call: driver_call,
        on_event: driver_on_event,
        on_next: driver_on_next,
        teardown: driver_teardown,
    };
    for (table, seen) in [(&V4_TABLE, 4), (&PREVIOUS_TABLE, ABI_VERSION - 1)] {
        let message = from_static(table)
            .expect_err("earlier table rejected")
            .to_string();
        assert!(
            message.contains(&format!("plugin exports {seen}")),
            "{message}"
        );
        assert!(
            message.contains(&format!("host expects {ABI_VERSION}")),
            "{message}"
        );
    }
}

/// E1.12: concurrent background dispatches from many plugin threads all
/// settle with correct identity replies (A3 thread safety).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_dispatches_settle_independently() {
    let ctx = boot().await;
    let mut handles = Vec::new();
    for worker in 0..4_u64 {
        let ctx = ctx.clone();
        handles.push(tokio::spawn(async move {
            let mut collected = Vec::new();
            for job in 0..4_u64 {
                let nonce = (0xE100 + (worker << 8)) | job;
                let (status, reply) = dispatch_bg(
                    &ctx,
                    nonce,
                    "evt-nobody",
                    MODE_WATERFALL,
                    json!({ "w": worker, "j": job }),
                )
                .await;
                collected.push((status, reply));
            }
            collected
        }));
    }
    for collected in handles {
        for (status, reply) in collected.await.expect("worker joins") {
            assert_eq!(status, STATUS_OK);
            let value: Value = serde_json::from_slice(&reply).expect("reply parses");
            assert!(value.get("w").is_some(), "identity reply {value}");
        }
    }
}

/// E1.13: `MODE_SERIAL` runs listeners in order and stops at the first
/// non-null value, which rides back to the dispatcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_mode_stops_at_first_non_null_listener() {
    let ctx = boot_with_in_process_listener("evt-serial", Value::Null).await;
    // The null-returning in-process listener runs first and lets the chain
    // continue; the bridged listener's output is non-null and bails.
    let (status, reply) = dispatch_join(&ctx, "evt-serial", MODE_SERIAL, json!({})).await;
    assert_eq!(status, STATUS_OK);
    let value: Value = serde_json::from_slice(&reply).expect("reply parses");
    assert_eq!(value, json!({ "serial-bail": true }));
}

/// E1.14: `MODE_BAIL` is first-non-null-wins over the same fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bail_mode_returns_first_non_null_listener() {
    let ctx = boot_with_in_process_listener("evt-serial", Value::Null).await;
    // The null in-process arm continues the chain; the bridged listener's
    // non-null output is the first bail value and wins.
    let (status, reply) = dispatch_join(&ctx, "evt-serial", MODE_BAIL, json!({})).await;
    assert_eq!(status, STATUS_OK);
    let value: Value = serde_json::from_slice(&reply).expect("reply parses");
    assert_eq!(value, json!({ "serial-bail": true }));
}

/// E1.15: `MODE_PARALLEL` runs listeners concurrently and aggregates a
/// listener failure into a request-visible failure (B3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_mode_aggregates_listener_failures() {
    let ctx = Context::new();
    let failing: Listener = Arc::new(|_ctx, _payload, _next| {
        Box::pin(async move { Err(Error::new("boom", "parallel listener failed")) })
    });
    ctx.on("evt-parallel", failing)
        .await
        .expect("listener registers");
    let driver: Arc<dyn Plugin> = Arc::new(from_static(&DRIVER_TABLE).expect("driver table"));
    start(&ctx, driver).await;
    let (status, reply) = dispatch_join(&ctx, "evt-parallel", MODE_PARALLEL, json!({})).await;
    assert_eq!(status, STATUS_FAILED);
    let detail: Value = serde_json::from_slice(&reply).expect("detail parses");
    // Parallel aggregates per-listener failures under the kernel's
    // aggregate code; the original cause stays visible in the message.
    assert_eq!(detail["code"], "aggregate");
    assert!(String::from_utf8_lossy(reply.as_slice()).contains("boom"));
}

/// E1.16: `MODE_EMIT` enqueues and returns without writing the reply; the
/// listener observes the event asynchronously.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_mode_enqueues_and_returns() {
    let ctx = boot().await;
    let hits = Arc::new(AtomicU64::new(0));
    let inner = Arc::clone(&hits);
    let listener: Listener = Arc::new(move |_ctx, _payload, _next| {
        let hits = Arc::clone(&inner);
        Box::pin(async move {
            hits.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        })
    });
    ctx.on("evt-emit", listener)
        .await
        .expect("listener registers");
    let envelope = json!({ "event": "evt-emit", "mode": MODE_EMIT, "body": {} });
    let (status, reply) =
        call_driver(&ctx, DRIVER_DISPATCH_JOIN, envelope.to_string().as_bytes()).await;
    assert_eq!(status, STATUS_OK);
    assert!(reply.is_empty(), "emit mode writes no reply");
    for _ in 0..200 {
        if hits.load(Ordering::SeqCst) >= 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("listener never observed the emit-mode event");
}
