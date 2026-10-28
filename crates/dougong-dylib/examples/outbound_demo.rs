// Rust guideline compliant 2026-10-28
//! ABI v3/v4 outbound acceptance demo: the greeter dylib's own worker thread
//! performs a synchronous cross-plugin `require` and publishes an event
//! (the full chain), then embedded arms prove grants and deterministic
//! failure modes of the vtable contract:
//!
//! - grant: greeter authorizes a second dylib-form plugin's activation,
//!   which then requires greeter's service without a static injection;
//! - permission: requiring a service absent from the descriptor `inject`
//!   list fails with `STATUS_UNDECLARED` (capability error);
//! - reentrancy: requiring a service that calls back into greeter on the
//!   same thread fails with `STATUS_REENTRANCY` instead of deadlocking the
//!   per-instance lock.
//! - cross-instance: two dylib-form instances whose gated handlers require
//!   through an in-process relay close a wait cycle across threads; the
//!   arming that completes the cycle fails with `STATUS_REENTRANCY` while
//!   the other leg unwinds (D-V3-3, 2026-10-25 amendment).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use dougong::{Context, Error, Invoke, Invoker, LogLevel, plugin};
use dougong_abi::{
    HostExports, PluginExports, STATUS_CAPACITY, STATUS_OK, STATUS_REENTRANCY, STATUS_UNDECLARED,
    write_out,
};
use dougong_dylib::{from_static, load};
use greeter_plugin::{GREETER_GRANT, GREETER_HELLO, GREETER_REQUIRE, GREETER_WORKER_REPLY};
use serde_json::Value;

fn plugin_path() -> PathBuf {
    // The cdylib lands next to the other workspace artifacts.
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend(["../../", "target", "debug"]);
    let name = if cfg!(target_os = "macos") {
        "libgreeter_plugin.dylib"
    } else if cfg!(windows) {
        "greeter_plugin.dll"
    } else {
        "libgreeter_plugin.so"
    };
    path.join(name)
}

/// The declared dependency: echoes `oracle:<payload>`, except that the
/// payload `loop` calls back into greeter's own `greeting` service. That
/// callback is what closes the A -> oracle -> A reentrancy cycle whenever
/// the caller sits inside a greeter export (the `GREETER_REQUIRE` handler).
struct Oracle {
    ctx: Context,
}

impl Invoke for Oracle {
    fn call(&self, _method: u32, payload: Vec<u8>) -> dougong::BoxFuture<dougong::Result<Vec<u8>>> {
        if payload.as_slice() == b"loop" {
            let Some(invoker) = self.ctx.get::<Invoker>("greeting") else {
                return Box::pin(async { Err(Error::msg("greeting is not committed")) });
            };
            return invoker.call(GREETER_HELLO, b"loopback".to_vec());
        }
        Box::pin(async move { Ok([b"oracle:".as_ref(), &payload].concat()) })
    }
}

/// Unpacks the `i32 little-endian status || payload` framing that
/// [`GREETER_REQUIRE`] documents.
fn unpack_framed(bytes: &[u8]) -> dougong::Result<(i32, &[u8])> {
    if bytes.len() < 4 {
        return Err(Error::msg("framed reply shorter than the status prefix"));
    }
    let (head, payload) = bytes.split_at(4);
    let head: [u8; 4] = head
        .try_into()
        .map_err(|err| Error::msg(format!("framing prefix is not four bytes: {err}")))?;
    Ok((i32::from_le_bytes(head), payload))
}

/// Parses the JSON detail document a failure status carries.
fn detail_code(payload: &[u8]) -> dougong::Result<String> {
    let detail: Value = serde_json::from_slice(payload)
        .map_err(|err| Error::msg(format!("detail is not JSON: {err}")))?;
    detail
        .get("code")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::msg("detail document lacks a \"code\" field"))
}

// ---------------------------------------------------------------------------
// Grant arm fixture: a second dylib-form plugin with no static injection.
// ---------------------------------------------------------------------------

struct CallbackState {
    host: HostExports,
}

unsafe extern "C" fn callback_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract.
    if unsafe { write_out(b"grant-callback", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn callback_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract. The empty inject list is the arm.
    if unsafe {
        write_out(
            br#"{"name":"grant-callback","provide":["grant-callback"],"inject":[]}"#,
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

unsafe extern "C" fn callback_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of setup; copied by value.
    let host = unsafe { *host };
    Box::into_raw(Box::new(CallbackState { host })).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn callback_call(
    handle: *mut std::ffi::c_void,
    _method: u32,
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
    let state = unsafe { &mut *handle.cast::<CallbackState>() };
    let payload = if data.is_null() {
        &[]
    } else {
        // SAFETY: the vtable contract makes data readable for len bytes here.
        unsafe { std::slice::from_raw_parts(data, len) }
    };
    let (status, reply) = plugin_require(&state.host, b"greeting", GREETER_HELLO, payload);
    pack_framed(status, &reply, out, cap, out_len)
}

unsafe extern "C" fn callback_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: allocated by callback_setup and torn down exactly once.
    drop(unsafe { Box::from_raw(handle.cast::<CallbackState>()) });
}

static CALLBACK_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: callback_name,
    descriptor: callback_descriptor,
    setup: callback_setup,
    call: callback_call,
    on_event: ring_on_event,
    on_next: ring_on_next,
    teardown: callback_teardown,
};

// ---------------------------------------------------------------------------
// Cross-instance arm fixtures: two statically-resolved dylib-form instances
// (`from_static` runs the same ABI gate and descriptor checks as `load`)
// whose gated call handlers require through the relay service below.
// ---------------------------------------------------------------------------

/// Gate shared with a ring handler: the handler parks inside its gated
/// export until the demo opens the gate (after both instance locks are
/// held).
struct RingCtl {
    proceed: (Mutex<bool>, Condvar),
}

impl RingCtl {
    const fn new() -> Self {
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

/// `Send` wrapper for a copied vtable: the ABI contract lets `require` run
/// from any plugin thread, concurrently (A5).
struct SendVtable(HostExports);
// SAFETY: the vtable contract states the host entries are callable from any
// thread concurrently; the host implementations contain their panics.
unsafe impl Send for SendVtable {}

static RING1_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: ring1_name,
    descriptor: ring1_descriptor,
    setup: ring1_setup,
    call: ring1_call,
    on_event: ring_on_event,
    on_next: ring_on_next,
    teardown: ring_teardown,
};

static RING2_TABLE: PluginExports = PluginExports {
    abi_version: dougong_abi::ABI_VERSION,
    name: ring2_name,
    descriptor: ring2_descriptor,
    setup: ring2_setup,
    call: ring2_call,
    on_event: ring_on_event,
    on_next: ring_on_next,
    teardown: ring_teardown,
};

static RING1_HOST: Mutex<Option<SendVtable>> = Mutex::new(None);
static RING2_HOST: Mutex<Option<SendVtable>> = Mutex::new(None);
static RING1_CTL: Mutex<Option<Arc<RingCtl>>> = Mutex::new(None);
static RING2_CTL: Mutex<Option<Arc<RingCtl>>> = Mutex::new(None);
static RING1_ENTERED: AtomicBool = AtomicBool::new(false);
static RING2_ENTERED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn ring1_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract.
    if unsafe { write_out(b"ring1", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn ring1_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract.
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
    // SAFETY: caller-owned buffer contract.
    if unsafe { write_out(b"ring2", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn ring2_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: caller-owned buffer contract.
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
    // SAFETY: valid for the duration of `setup`; copied by value (v3).
    *RING1_HOST.lock().expect("ring1 vtable slot") = Some(SendVtable(unsafe { *host }));
    Box::into_raw(Box::new(0u8)).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn ring2_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    // SAFETY: as `ring1_setup`.
    *RING2_HOST.lock().expect("ring2 vtable slot") = Some(SendVtable(unsafe { *host }));
    Box::into_raw(Box::new(0u8)).cast::<std::ffi::c_void>()
}

unsafe extern "C" fn ring_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: allocated by the ring setup for this activation; teardown
    // runs exactly once per successful setup.
    drop(unsafe { Box::from_raw(handle.cast::<u8>()) });
}

/// Gated ring handler: announce entry, park until the gate opens, then
/// require through the relay and frame the require outcome back.
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
        .expect("ring1 vtable slot")
        .as_ref()
        .expect("ring1 vtable copied at setup")
        .0;
    let ctl = Arc::clone(
        RING1_CTL
            .lock()
            .expect("ring1 gate slot")
            .as_ref()
            .expect("ring1 gate armed"),
    );
    RING1_ENTERED.store(true, Ordering::SeqCst);
    ctl.wait();
    let (status, reply) = plugin_require(&host, b"svc-bridge", 0, b"ring1");
    pack_framed(status, &reply, out, cap, out_len)
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
        .expect("ring2 vtable slot")
        .as_ref()
        .expect("ring2 vtable copied at setup")
        .0;
    let ctl = Arc::clone(
        RING2_CTL
            .lock()
            .expect("ring2 gate slot")
            .as_ref()
            .expect("ring2 gate armed"),
    );
    RING2_ENTERED.store(true, Ordering::SeqCst);
    ctl.wait();
    let (status, reply) = plugin_require(&host, b"svc-bridge", 1, b"ring2");
    pack_framed(status, &reply, out, cap, out_len)
}

const unsafe extern "C" fn ring_on_event(
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
    STATUS_OK
}

const unsafe extern "C" fn ring_on_next(
    _handle: *mut std::ffi::c_void,
    _event: u32,
    _chain_status: i32,
    _data: *const u8,
    _len: usize,
    _out: *mut u8,
    _cap: usize,
    _out_len: *mut usize,
) -> i32 {
    STATUS_OK
}

/// Plugin-side growable `require`: retries with a doubled buffer whenever
/// the host asks for more room (the caller-owned buffer protocol).
fn plugin_require(
    host: &HostExports,
    service: &[u8],
    method: u32,
    payload: &[u8],
) -> (i32, Vec<u8>) {
    let mut cap = 4096;
    loop {
        let mut buf = vec![0u8; cap];
        let mut len = 0usize;
        // SAFETY: the vtable is live until this activation's teardown
        // returns (the demo holds both instances until after the
        // assertions); buffers are caller-owned.
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
        if status != STATUS_CAPACITY {
            buf.truncate(len);
            return (status, buf);
        }
        cap *= 2;
    }
}

/// Packs `i32 little-endian status || payload` into the caller-owned out
/// buffer (the framing the ring handlers document for their replies).
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

/// In-process relay: method 0 requires `svc-ring2`, method 1 requires
/// `svc-ring1` — the shared leg both ring handlers route through.
struct BridgeSvc {
    ctx: Context,
}

impl Invoke for BridgeSvc {
    fn call(&self, method: u32, payload: Vec<u8>) -> dougong::BoxFuture<dougong::Result<Vec<u8>>> {
        let target = if method == 0 {
            "svc-ring2"
        } else {
            "svc-ring1"
        };
        let Some(invoker) = self.ctx.get::<Invoker>(target) else {
            return Box::pin(async move { Err(Error::msg(format!("{target} is not committed"))) });
        };
        invoker.call(0, payload)
    }
}

/// Polls `check` until it holds (bounded: a demo arm that never settles
/// must fail, not hang).
async fn wait_for(check: impl Fn() -> bool) -> dougong::Result<()> {
    for _ in 0..500 {
        if check() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(Error::msg("arm did not settle within the poll budget"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = plugin_path();
    let greeter: Arc<dyn dougong::Plugin> = load(&path)?;

    // Multi-threaded on purpose: the cross-instance arm parks foreign
    // calls inside gates that this body opens afterwards, so the runtime
    // needs workers beside the one blocked in a synchronous export (the
    // vtable contract's blocking model).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let ctx = Context::new();
        let logger = ctx.logger();
        let log = |message: String| {
            logger.log(LogLevel::Info, "outbound-demo", &message);
        };

        log("-- loaded dylib plugin --".to_string());

        // The oracle must be committed before greeter activates: the
        // plugin's worker thread requires it from `setup` time onward.
        let oracle = plugin("oracle", vec![], |ctx, _config| {
            Box::pin(async move {
                let service = Oracle { ctx: ctx.clone() };
                ctx.provide("greeter.oracle", Invoker::new(Arc::new(service)))
                    .await?;
                Ok(())
            })
        });
        let oracle_fiber = ctx.plugin(oracle, None)?;
        oracle_fiber.join().await?;

        // Registered before greeter loads: the worker's `greeter/ready` emit
        // is fire-and-forget, so the listener must already be in place.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Value>();
        let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
        ctx.on(
            "greeter/ready",
            Arc::new(move |_ctx, payload, _next| {
                let ready_tx = Arc::clone(&ready_tx);
                Box::pin(async move {
                    if let Ok(mut slot) = ready_tx.lock()
                        && let Some(tx) = slot.take()
                    {
                        let _dropped = tx.send(payload);
                    }
                    Ok(Value::Null)
                })
            }),
        )
        .await?;

        let greeter_fiber = ctx.plugin(greeter, None)?;
        greeter_fiber.join().await?;

        // Full chain: the worker thread's emit arrived, and its synchronous
        // require reply is read back through greeter's own service.
        let ready = tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .map_err(|err| Error::msg(format!("greeter/ready did not arrive: {err}")))?
            .map_err(|err| Error::msg(format!("greeter/ready listener dropped: {err}")))?;
        assert_eq!(
            ready.get("source").and_then(Value::as_str),
            Some("greeter-worker")
        );
        let greeting = ctx
            .get::<Invoker>("greeting")
            .ok_or_else(|| Error::msg("greeting is not committed"))?;
        let reply = greeting.call(GREETER_WORKER_REPLY, Vec::new()).await?;
        assert_eq!(reply, b"oracle:worker-hello");
        log("-- full chain: own-thread require + emit verified --".to_string());

        // Permission arm: `unlisted` is not in greeter's descriptor `inject`
        // list, so the require is rejected before any resolution happens.
        let denied = greeting
            .call(GREETER_REQUIRE, b"unlisted\0probe".to_vec())
            .await?;
        let (status, detail) = unpack_framed(&denied)?;
        assert_eq!(status, STATUS_UNDECLARED);
        assert_eq!(detail_code(detail)?, "undeclared_dependency");
        log("-- permission arm: undeclared require rejected --".to_string());

        // Grant arm: the callback plugin has an empty descriptor inject set.
        // Greeter grants its activation `greeting`; the subsequent callback
        // therefore takes the grant path rather than the static-inject path.
        let callback: Arc<dyn dougong::Plugin> = Arc::new(from_static(&CALLBACK_TABLE)?);
        let callback_fiber = ctx.plugin(callback, None)?;
        callback_fiber.join().await?;
        let granted = greeting
            .call(GREETER_GRANT, b"grant-callback\0greeting".to_vec())
            .await?;
        let (status, _) = unpack_framed(&granted)?;
        assert_eq!(status, STATUS_OK);
        let callback_reply = ctx
            .get::<Invoker>("grant-callback")
            .ok_or_else(|| Error::msg("grant-callback is not committed"))?
            .call(0, b"grant-round-trip".to_vec())
            .await?;
        let (status, payload) = unpack_framed(&callback_reply)?;
        assert_eq!(status, STATUS_OK);
        assert_eq!(payload, b"hello from a dougong dylib");
        callback_fiber.dispose().await;
        log("-- grant arm: authorized callback round trip verified --".to_string());

        // Reentrancy arm: the `GREETER_REQUIRE` handler runs inside a greeter
        // export, so the oracle's `loop` callback into `greeting` would
        // re-enter the same instance on the same thread. The timeout is the
        // regression guard: a deadlock fails this demo instead of hanging it.
        let cyclic = tokio::time::timeout(
            Duration::from_secs(5),
            greeting.call(GREETER_REQUIRE, b"greeter.oracle\0loop".to_vec()),
        )
        .await
        .map_err(|err| Error::msg(format!("reentrancy arm deadlocked: {err}")))??;
        let (status, detail) = unpack_framed(&cyclic)?;
        assert_eq!(status, STATUS_REENTRANCY);
        assert_eq!(detail_code(detail)?, "reentrancy_cycle");
        log("-- reentrancy arm: cycle rejected deterministically --".to_string());

        cross_instance_arm(&ctx, &log).await?;

        greeter_fiber.dispose().await;
        ctx.stop().await;
        log("-- outbound demo done --".to_string());
        Ok::<(), Error>(())
    })?;
    Ok(())
}

/// The cross-instance arm: two gated dylib-form instances whose handlers
/// require through the relay, closing a wait cycle across threads. Asserts
/// the deterministic outcomes (exactly one reentrancy status; the other
/// leg unwinds through the relay with the nested detail inside) and logs
/// the arm result.
async fn cross_instance_arm(
    ctx: &Context,
    log: &(dyn Fn(String) + Send + Sync),
) -> dougong::Result<()> {
    // Cross-instance arm: both ring handlers park inside their gated
    // exports holding their instance locks; opening the gates builds
    // the wait cycle ring1 -> ring2 -> ring1. The arming that completes
    // the cycle must fail with STATUS_REENTRANCY on that thread while
    // the other leg unwinds through the relay — never a deadlock.
    let bridge = plugin("bridge", vec![], |ctx, _config| {
        Box::pin(async move {
            let svc = BridgeSvc { ctx: ctx.clone() };
            ctx.provide("svc-bridge", Invoker::new(Arc::new(svc)))
                .await?;
            Ok(())
        })
    });
    ctx.plugin(bridge, None)?.join().await?;
    *RING1_CTL.lock().expect("ring1 gate slot") = Some(Arc::new(RingCtl::new()));
    let ring1: Arc<dyn dougong::Plugin> = Arc::new(from_static(&RING1_TABLE)?);
    ctx.plugin(ring1, None)?.join().await?;
    *RING2_CTL.lock().expect("ring2 gate slot") = Some(Arc::new(RingCtl::new()));
    let ring2: Arc<dyn dougong::Plugin> = Arc::new(from_static(&RING2_TABLE)?);
    ctx.plugin(ring2, None)?.join().await?;

    let leg1 = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            let invoker = ctx
                .get::<Invoker>("svc-ring1")
                .ok_or_else(|| Error::msg("svc-ring1 is not committed"))?;
            let reply = invoker.call(0, b"ping".to_vec()).await?;
            unpack_framed(&reply).map(|(status, payload)| (status, payload.to_vec()))
        }
    });
    let leg2 = tokio::spawn({
        let ctx = ctx.clone();
        async move {
            let invoker = ctx
                .get::<Invoker>("svc-ring2")
                .ok_or_else(|| Error::msg("svc-ring2 is not committed"))?;
            let reply = invoker.call(0, b"ping".to_vec()).await?;
            unpack_framed(&reply).map(|(status, payload)| (status, payload.to_vec()))
        }
    });
    wait_for(|| RING1_ENTERED.load(Ordering::SeqCst)).await?;
    wait_for(|| RING2_ENTERED.load(Ordering::SeqCst)).await?;
    RING1_CTL
        .lock()
        .expect("ring1 gate slot")
        .as_ref()
        .expect("ring1 gate armed")
        .open();
    RING2_CTL
        .lock()
        .expect("ring2 gate slot")
        .as_ref()
        .expect("ring2 gate armed")
        .open();

    let reply1 = tokio::time::timeout(Duration::from_secs(5), leg1)
        .await
        // timeout: the deadlock guard
        .map_err(|err| Error::msg(format!("ring1 leg deadlocked: {err}")))?
        // join: a panicked task must fail the arm, not pass silently
        .map_err(|err| Error::msg(format!("ring1 leg panicked: {err}")))?
        .map_err(|err| Error::msg(format!("ring1 leg failed: {err}")))?;
    let (status1, payload1) = reply1;
    let reply2 = tokio::time::timeout(Duration::from_secs(5), leg2)
        .await
        // timeout: the deadlock guard
        .map_err(|err| Error::msg(format!("ring2 leg deadlocked: {err}")))?
        // join: a panicked task must fail the arm, not pass silently
        .map_err(|err| Error::msg(format!("ring2 leg panicked: {err}")))?
        .map_err(|err| Error::msg(format!("ring2 leg failed: {err}")))?;
    let (status2, payload2) = reply2;
    // Exactly one arming completes the cycle: that leg gets the direct
    // reentrancy status. The loser's parked leg unwinds through the
    // relay, which carries the inner framing as opaque bytes — its
    // top-level status stays OK with the nested detail inside.
    let mut statuses = [status1, status2];
    statuses.sort_unstable();
    assert_eq!(statuses, [STATUS_REENTRANCY, STATUS_OK]);
    for (status, payload) in [(&status1, &payload1), (&status2, &payload2)] {
        let detail: Value = if *status == STATUS_REENTRANCY {
            serde_json::from_slice(payload)
                .map_err(|err| Error::msg(format!("framed detail is not JSON: {err}")))?
        } else {
            serde_json::from_slice(&payload[4..])
                .map_err(|err| Error::msg(format!("nested detail is not JSON: {err}")))?
        };
        assert_eq!(
            detail.get("code").and_then(Value::as_str),
            Some("reentrancy_cycle")
        );
    }
    log("-- cross-instance arm: wait cycle closed deterministically --".to_string());
    Ok(())
}
