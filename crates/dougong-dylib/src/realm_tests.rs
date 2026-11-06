//! Outbound realm activation tests (ABI v6, D33 /
//! `realm_requirements.md` §3 E1).
//!
//! Fixtures mirror `dispatch_tests`: static export tables embedded in the
//! test binary. The spawner fixture reaches the realm vtable entries from
//! plugin-owned threads (the posture the contract demands) and from a foreign
//! entry thread (the rejection arm); child artifacts are resolved through the
//! test source table instead of `dlopen`. Every test holds one process-wide
//! lock: the spawn-link table is global, and the assertions about its
//! contents (and the shared teardown log) need a quiet process.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dougong::{Context, FiberHandle, Invoker, Plugin};
use dougong_abi::{
    ABI_VERSION, HostExports, PluginExports, REALM_SPAWN_REPLY_MIN, STATUS_CAPACITY, STATUS_FAILED,
    STATUS_INACTIVE, STATUS_OK, STATUS_REENTRANCY, STATUS_UNDECLARED, write_out,
};
use serde_json::{Value, json};

use super::realm::{links_snapshot, register_test_source};
use super::{arm_deadlock_watchdog, from_static};

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

/// `Send` wrapper for a copied vtable: the realm entries are contractually
/// callable from any plugin-owned thread, and the fixtures hand the copy to
/// spawned threads only.
struct SendHost(HostExports);
// SAFETY: the vtable contract states the entries are callable from any
// plugin-owned thread concurrently; the host implementations are `Send + Sync`.
unsafe impl Send for SendHost {}

fn plugin_spawn(host: &SendHost, realm: u64, path: &str, cap: usize) -> (i32, Vec<u8>) {
    let mut buf = vec![0_u8; cap];
    let mut len = 0_usize;
    // SAFETY: the vtable is live (fixtures never outlive their activation);
    // buffers are caller-owned.
    let status = unsafe {
        (host.0.realm_spawn)(
            host.0.host_handle,
            realm,
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

fn plugin_retire(host: &SendHost, child: u64) -> i32 {
    // SAFETY: the vtable is live; the entry reads no caller buffers.
    unsafe { (host.0.realm_retire)(host.0.host_handle, child) }
}

fn plugin_realm_require(
    host: &SendHost,
    realm: u64,
    service: &str,
    method: u32,
    payload: &[u8],
) -> (i32, Vec<u8>) {
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
// Descriptor plumbing and the event entries these fixtures never use
// ---------------------------------------------------------------------------

macro_rules! descriptor_fns {
    ($name_fn:ident, $desc_fn:ident, $name:literal, $descriptor:literal) => {
        unsafe extern "C" fn $name_fn(out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
            // SAFETY: host buffer contract.
            if unsafe { write_out($name, out, cap, out_len) } {
                STATUS_OK
            } else {
                STATUS_CAPACITY
            }
        }

        unsafe extern "C" fn $desc_fn(out: *mut u8, cap: usize, out_len: *mut usize) -> i32 {
            // SAFETY: host buffer contract.
            if unsafe { write_out($descriptor, out, cap, out_len) } {
                STATUS_OK
            } else {
                STATUS_CAPACITY
            }
        }
    };
}

unsafe extern "C" fn listens_to_nothing(
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

unsafe extern "C" fn continues_nothing(
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

/// Teardown order log shared by the fixtures (tests run serialized).
static LOG: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

fn log(entry: &'static str) {
    LOG.lock().expect("teardown log").push(entry);
}

// ---------------------------------------------------------------------------
// Child fixtures
// ---------------------------------------------------------------------------

static NEXT_CHILD_ID: AtomicU64 = AtomicU64::new(1);

struct ChildState {
    id: u64,
}

descriptor_fns!(
    child_name,
    child_descriptor,
    b"realm-child",
    br#"{"name":"realm-child","provide":["svc-child"],"inject":[],"listen":{}}"#
);

descriptor_fns!(
    pending_name,
    pending_descriptor,
    b"realm-pending",
    br#"{"name":"realm-pending","provide":["svc-pending"],"inject":["svc-child"],"listen":{}}"#
);

descriptor_fns!(
    failing_name,
    failing_descriptor,
    b"realm-failing",
    br#"{"name":"realm-failing","provide":["svc-failing"],"inject":[],"listen":{}}"#
);

unsafe extern "C" fn child_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(ChildState {
        id: NEXT_CHILD_ID.fetch_add(1, Ordering::SeqCst),
    }))
    .cast::<std::ffi::c_void>()
}

unsafe extern "C" fn failing_setup(_host: *const HostExports) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn child_call(
    handle: *mut std::ffi::c_void,
    _method: u32,
    payload: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if handle.is_null() {
        return STATUS_FAILED;
    }
    // SAFETY: the handle came from `child_setup` and the per-instance lock
    // serializes every foreign entry touching it.
    let state = unsafe { &*(handle.cast::<ChildState>()) };
    // SAFETY: vtable input contract (the payload slice is call-scoped).
    let bytes = unsafe { std::slice::from_raw_parts(payload, len) };
    let reply = format!("child#{}:{}", state.id, String::from_utf8_lossy(bytes));
    // SAFETY: host buffer contract.
    if unsafe { write_out(reply.as_bytes(), out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn child_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    log("child-teardown");
    // SAFETY: the handle came from `child_setup` and is torn down once.
    drop(unsafe { Box::from_raw(handle.cast::<ChildState>()) });
}

static CHILD_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: child_name,
    descriptor: child_descriptor,
    setup: child_setup,
    call: child_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: child_teardown,
};

static PENDING_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: pending_name,
    descriptor: pending_descriptor,
    setup: child_setup,
    call: child_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: child_teardown,
};

descriptor_fns!(
    child2_name,
    child2_descriptor,
    b"realm-child2",
    br#"{"name":"realm-child2","provide":["svc-child2"],"inject":[],"listen":{}}"#
);

static CHILD2_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: child2_name,
    descriptor: child2_descriptor,
    setup: child_setup,
    call: child_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: child_teardown,
};

// ---------------------------------------------------------------------------
// Shadowing spawner: injects the child's service name on purpose (documents
// the fail-closed shadowing arm)
// ---------------------------------------------------------------------------

descriptor_fns!(
    shadow_name,
    shadow_descriptor,
    b"shadow-spawner",
    br#"{"name":"shadow-spawner","provide":["svc-shadow"],"inject":["svc-child"],"listen":{}}"#
);

static SHADOWING_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: shadow_name,
    descriptor: shadow_descriptor,
    setup: spawner_setup,
    call: spawner_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: spawner_teardown,
};

// ---------------------------------------------------------------------------
// Setup-time spawner: starts a plugin-owned thread inside `setup` that
// immediately realm-spawns a child (the C2 ordering case)
// ---------------------------------------------------------------------------

static SETUP_SPAWN_RESULT: Mutex<Option<(i32, Vec<u8>)>> = Mutex::new(None);

struct SetupSpawnerState {
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

descriptor_fns!(
    setup_spawner_name,
    setup_spawner_descriptor,
    b"setup-spawner",
    br#"{"name":"setup-spawner","provide":["svc-setup-spawner"],"inject":[],"listen":{}}"#
);

unsafe extern "C" fn setup_spawner_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of setup; copied by value.
    let host = SendHost(unsafe { *host });
    let state = Box::into_raw(Box::new(SetupSpawnerState {
        thread: Mutex::new(None),
    }));
    // A plugin-owned thread (empty entry stack) may spawn during the
    // spawner's own setup: the parent's fiber is Loading, which accepts
    // effects, so the child's cascade effect registers after the
    // `dylib.teardown` effect and therefore drains BEFORE the teardown
    // export.
    let thread = std::thread::spawn(move || {
        let outcome = plugin_spawn(&host, 0, "child", 4096);
        *SETUP_SPAWN_RESULT.lock().expect("setup spawn result") = Some(outcome);
    });
    // SAFETY: the state was just boxed and is exclusively ours.
    unsafe { &*state }
        .thread
        .lock()
        .expect("setup spawner thread slot")
        .replace(thread);
    state.cast::<std::ffi::c_void>()
}

unsafe extern "C" fn setup_spawner_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    log("setup-spawner-teardown");
    // SAFETY: the handle came from `setup_spawner_setup` and is torn down
    // once; joining the setup thread upholds the thread discipline.
    let state = unsafe { Box::from_raw(handle.cast::<SetupSpawnerState>()) };
    state
        .thread
        .lock()
        .expect("setup spawner thread slot")
        .take()
        .expect("setup thread exists")
        .join()
        .expect("setup thread joins");
}

static SETUP_SPAWNER_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: setup_spawner_name,
    descriptor: setup_spawner_descriptor,
    setup: setup_spawner_setup,
    call: spawner_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: setup_spawner_teardown,
};

static FAILING_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: failing_name,
    descriptor: failing_descriptor,
    setup: failing_setup,
    call: child_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: child_teardown,
};

/// A child artifact built against the previous ABI revision.
static STALE_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION - 1,
    name: child_name,
    descriptor: child_descriptor,
    setup: child_setup,
    call: child_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: child_teardown,
};

// ---------------------------------------------------------------------------
// Spawner fixture: provides svc-spawner and reaches the realm vtable entries
// ---------------------------------------------------------------------------

const OP_SPAWN: u32 = 0;
const OP_SPAWN_IN_ENTRY: u32 = 1;
const OP_RETIRE: u32 = 2;
const OP_RETIRE_IN_ENTRY: u32 = 3;
const OP_REQUIRE: u32 = 4;
const OP_CONCURRENT: u32 = 5;
const OP_HAMMER: u32 = 6;

/// Last status the hammer thread observed (`i32::MIN` before it ran).
static HAMMER_LAST: AtomicI32 = AtomicI32::new(i32::MIN);

struct Hammer {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

struct SpawnerState {
    host: SendHost,
    hammer: Mutex<Option<Hammer>>,
}

descriptor_fns!(
    spawner_name,
    spawner_descriptor,
    b"realm-spawner",
    br#"{"name":"realm-spawner","provide":["svc-spawner"],"inject":[],"listen":{}}"#
);

unsafe extern "C" fn spawner_setup(host: *const HostExports) -> *mut std::ffi::c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: valid for the duration of setup; copied by value.
    let host = unsafe { *host };
    Box::into_raw(Box::new(SpawnerState {
        host: SendHost(host),
        hammer: Mutex::new(None),
    }))
    .cast::<std::ffi::c_void>()
}

fn field_u64(request: &Value, key: &str) -> u64 {
    request[key].as_u64().unwrap_or(0)
}

fn field_str<'a>(request: &'a Value, key: &str) -> &'a str {
    request[key].as_str().unwrap_or_default()
}

/// Runs `work` on a fresh plugin-owned thread and joins it.
fn on_plugin_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(work).join().expect("plugin thread")
}

unsafe extern "C" fn spawner_call(
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
    // SAFETY: the handle came from `spawner_setup` and the per-instance lock
    // serializes every foreign entry touching it.
    let state = unsafe { &*(handle.cast::<SpawnerState>()) };
    // SAFETY: vtable input contract (the payload slice is call-scoped).
    let bytes = unsafe { std::slice::from_raw_parts(payload, len) };
    let request: Value = serde_json::from_slice(bytes).expect("test request parses");
    let host = SendHost(state.host.0);
    match method {
        OP_SPAWN => {
            let path = field_str(&request, "path").to_string();
            let realm = field_u64(&request, "realm");
            let reply_cap = usize::try_from(request["cap"].as_u64().unwrap_or(4096)).unwrap_or(0);
            let (status, reply) =
                on_plugin_thread(move || plugin_spawn(&host, realm, &path, reply_cap));
            pack(status, &reply, out, cap, out_len)
        }
        OP_SPAWN_IN_ENTRY => {
            let (status, reply) = plugin_spawn(
                &host,
                field_u64(&request, "realm"),
                field_str(&request, "path"),
                4096,
            );
            pack(status, &reply, out, cap, out_len)
        }
        OP_RETIRE => {
            let child = field_u64(&request, "child");
            let status = on_plugin_thread(move || plugin_retire(&host, child));
            pack(status, b"", out, cap, out_len)
        }
        OP_RETIRE_IN_ENTRY => {
            let status = plugin_retire(&host, field_u64(&request, "child"));
            pack(status, b"", out, cap, out_len)
        }
        OP_REQUIRE => {
            let realm = field_u64(&request, "realm");
            let service = field_str(&request, "service").to_string();
            let body = field_str(&request, "body").to_string();
            let (status, reply) = on_plugin_thread(move || {
                plugin_realm_require(&host, realm, &service, 0, body.as_bytes())
            });
            pack(status, &reply, out, cap, out_len)
        }
        OP_CONCURRENT => {
            let workers = field_u64(&request, "n");
            let path = field_str(&request, "path").to_string();
            let results: Vec<bool> = (0..workers)
                .map(|_| {
                    let host = SendHost(state.host.0);
                    let path = path.clone();
                    std::thread::spawn(move || concurrent_round(&host, &path))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().expect("worker thread"))
                .collect();
            let ok = results.iter().filter(|ok| **ok).count();
            pack(STATUS_OK, ok.to_string().as_bytes(), out, cap, out_len)
        }
        OP_HAMMER => {
            let realm = field_u64(&request, "realm");
            let service = field_str(&request, "service").to_string();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            HAMMER_LAST.store(i32::MIN, Ordering::SeqCst);
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    let (status, _) = plugin_realm_require(&host, realm, &service, 0, b"x");
                    HAMMER_LAST.store(status, Ordering::SeqCst);
                    if status != STATUS_OK {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            *state.hammer.lock().expect("hammer slot") = Some(Hammer { stop, thread });
            pack(STATUS_OK, b"hammering", out, cap, out_len)
        }
        _ => STATUS_FAILED,
    }
}

/// One concurrent round: spawn into a fresh realm, call the child, retire it.
fn concurrent_round(host: &SendHost, path: &str) -> bool {
    let (status, reply) = plugin_spawn(host, 0, path, 4096);
    if status != STATUS_OK {
        return false;
    }
    let reply: Value = serde_json::from_slice(&reply).expect("spawn reply parses");
    let (realm, child) = (field_u64(&reply, "realm"), field_u64(&reply, "child"));
    let (status, body) = plugin_realm_require(host, realm, "svc-child", 0, b"c");
    let answered = status == STATUS_OK && body.starts_with(b"child#");
    answered && plugin_retire(host, child) == STATUS_OK
}

unsafe extern "C" fn spawner_teardown(handle: *mut std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    log("spawner-teardown");
    // SAFETY: the handle came from `spawner_setup` and is torn down once.
    let state = unsafe { Box::from_raw(handle.cast::<SpawnerState>()) };
    // The thread discipline (A6): plugin-owned threads end before teardown
    // returns.
    let hammer = state.hammer.lock().expect("hammer slot").take();
    if let Some(hammer) = hammer {
        hammer.stop.store(true, Ordering::SeqCst);
        hammer.thread.join().expect("hammer thread joins");
    }
}

static SPAWNER_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: spawner_name,
    descriptor: spawner_descriptor,
    setup: spawner_setup,
    call: spawner_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: spawner_teardown,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Serializes the realm tests (global link table and teardown log).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn start(ctx: &Context, table: &'static PluginExports) -> FiberHandle {
    arm_deadlock_watchdog();
    let plugin: Arc<dyn Plugin> = Arc::new(from_static(table).expect("fixture table"));
    let handle = ctx.plugin(plugin, None).expect("spawn");
    // The suite runs many tests in parallel on shared CI cores; the realm
    // tests serialize on SERIAL, so a starved runtime may take a while to
    // schedule the activation. The watchdog (not this budget) is the
    // deadlock detector.
    tokio::time::timeout(Duration::from_secs(60), handle.join())
        .await
        .expect("activation settles")
        .expect("activation ok");
    handle
}

struct Rig {
    ctx: Context,
    spawner: FiberHandle,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

async fn boot() -> Rig {
    let serial = SERIAL.lock().await;
    register_test_source("child", &CHILD_TABLE);
    register_test_source("child2", &CHILD2_TABLE);
    SETUP_SPAWN_RESULT
        .lock()
        .expect("setup spawn result")
        .take();
    register_test_source("pending", &PENDING_TABLE);
    register_test_source("failing", &FAILING_TABLE);
    register_test_source("stale", &STALE_TABLE);
    LOG.lock().expect("teardown log").clear();
    let ctx = Context::new();
    let spawner = start(&ctx, &SPAWNER_TABLE).await;
    Rig {
        ctx,
        spawner,
        _serial: serial,
    }
}

/// Calls a spawner op through the dynamic channel with a deadlock guardrail,
/// then unpacks the test framing.
async fn call_spawner(ctx: &Context, op: u32, request: Value) -> (i32, Vec<u8>) {
    let invoker = ctx
        .get::<Invoker>("svc-spawner")
        .expect("spawner committed");
    let bytes = tokio::time::timeout(
        Duration::from_secs(30),
        invoker.call(op, request.to_string().into_bytes()),
    )
    .await
    .expect("no deadlock")
    .expect("call ok");
    let (status, reply) = unpack(&bytes);
    (status, reply.to_vec())
}

fn parse(reply: &[u8]) -> Value {
    serde_json::from_slice(reply)
        .unwrap_or_else(|err| panic!("reply is JSON ({err}): {}", String::from_utf8_lossy(reply)))
}

/// Spawns `path` through the spawner and returns the parsed success reply.
async fn spawn_ok(ctx: &Context, realm: u64, path: &str) -> Value {
    let (status, reply) =
        call_spawner(ctx, OP_SPAWN, json!({ "path": path, "realm": realm })).await;
    assert_eq!(
        status,
        STATUS_OK,
        "spawn: {}",
        String::from_utf8_lossy(&reply)
    );
    parse(&reply)
}

async fn realm_require(ctx: &Context, realm: u64, service: &str, body: &str) -> (i32, Vec<u8>) {
    call_spawner(
        ctx,
        OP_REQUIRE,
        json!({ "realm": realm, "service": service, "body": body }),
    )
    .await
}

impl Rig {
    /// Disposes the spawner (children cascade first) within a budget.
    async fn finish(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.spawner.dispose())
            .await
            .expect("spawner tears down");
    }
}

// ---------------------------------------------------------------------------
// E1 matrix (realm_requirements.md §3)
// ---------------------------------------------------------------------------

/// E1.1: spawn a child into a fresh realm, then call its service through
/// `realm_require`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_answers_realm_require() {
    let rig = boot().await;
    let reply = spawn_ok(&rig.ctx, 0, "child").await;
    assert_eq!(reply["state"], "active");
    assert_eq!(reply["child"], 1);
    let realm = reply["realm"].as_u64().expect("realm label");
    assert_ne!(realm, 0, "a fresh realm is never the global label");
    let (status, body) = realm_require(&rig.ctx, realm, "svc-child", "hello").await;
    assert_eq!(status, STATUS_OK, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).ends_with(":hello"));
    rig.finish().await;
    drop(rig);
}

// ---------------------------------------------------------------------------
// PROBE (diagnostic, not for commit): the spawner itself provides the same
// service name globally that its realm children provide in the realm
// (the fr-agent-loop parent/child same-artifact shape).
// ---------------------------------------------------------------------------

descriptor_fns!(
    agentchild_name,
    agentchild_descriptor,
    b"realm-agentchild",
    br#"{"name":"realm-agentchild","provide":["svc-agent"],"inject":[],"listen":{}}"#
);

static AGENTCHILD_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: agentchild_name,
    descriptor: agentchild_descriptor,
    setup: child_setup,
    call: child_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: child_teardown,
};

descriptor_fns!(
    global_spawner_name,
    global_spawner_descriptor,
    b"global-spawner",
    br#"{"name":"global-spawner","provide":["svc-spawner","svc-agent"],"inject":[],"listen":{}}"#
);

static GLOBAL_SPAWNER_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: global_spawner_name,
    descriptor: global_spawner_descriptor,
    setup: spawner_setup,
    call: spawner_call,
    on_event: listens_to_nothing,
    on_next: continues_nothing,
    teardown: spawner_teardown,
};

/// E1.10 (regression): the spawner's own global provide of the same name
/// must not shadow the realm child — `realm_require` resolves realm-first
/// even when the caller's fiber holds a self-provide snapshot for the name
/// (fly-rafter's agent-loop parent/child same-artifact shape).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realm_child_wins_over_the_spawners_same_named_global_provide() {
    let serial = SERIAL.lock().await;
    register_test_source("agentchild", &AGENTCHILD_TABLE);
    LOG.lock().expect("teardown log").clear();
    let ctx = Context::new();
    let spawner = start(&ctx, &GLOBAL_SPAWNER_TABLE).await;
    let reply = spawn_ok(&ctx, 0, "agentchild").await;
    assert_eq!(reply["state"], "active");
    let realm = reply["realm"].as_u64().expect("realm");
    let (status, body) = realm_require(&ctx, realm, "svc-agent", "q").await;
    assert_eq!(
        status,
        STATUS_OK,
        "realm-first resolution: {:?}",
        String::from_utf8_lossy(&body)
    );
    assert!(String::from_utf8_lossy(&body).ends_with(":q"));
    tokio::time::timeout(Duration::from_secs(10), spawner.dispose())
        .await
        .expect("the spawner tears down");
    drop(ctx);
    drop(serial);
}

/// E1.2: the same artifact in two realms keeps the same service name without
/// a duplicate-provide collision, and each realm answers from its own child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realms_isolate_same_named_services() {
    let rig = boot().await;
    let first = spawn_ok(&rig.ctx, 0, "child").await;
    let second = spawn_ok(&rig.ctx, 0, "child").await;
    let (first_realm, second_realm) = (
        first["realm"].as_u64().expect("realm"),
        second["realm"].as_u64().expect("realm"),
    );
    assert_ne!(first_realm, second_realm);
    let (_, a) = realm_require(&rig.ctx, first_realm, "svc-child", "q").await;
    let (_, b) = realm_require(&rig.ctx, second_realm, "svc-child", "q").await;
    assert_ne!(a, b, "each realm answers from its own child activation");
    rig.finish().await;
    drop(rig);
}

/// E1.3: a realm-provided service is invisible to a global consumer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realm_services_stay_invisible_globally() {
    let rig = boot().await;
    spawn_ok(&rig.ctx, 0, "child").await;
    assert!(rig.ctx.get::<Invoker>("svc-child").is_none());
    rig.finish().await;
    drop(rig);
}

/// E1.4: a same-named global provider never widens `realm_require`: when the
/// realm lacks the service the global fallback resolves, and the call is
/// refused because that provider is not a child of the caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realm_require_refuses_non_child_providers() {
    let rig = boot().await;
    let _global = start(&rig.ctx, &CHILD_TABLE).await;
    let reply = spawn_ok(&rig.ctx, 0, "pending").await;
    assert_eq!(
        reply["state"], "active",
        "the global svc-child satisfies the inject"
    );
    let realm = reply["realm"].as_u64().expect("realm");
    let (status, _) = realm_require(&rig.ctx, realm, "svc-child", "q").await;
    assert_eq!(status, STATUS_UNDECLARED);
    let (status, body) = realm_require(&rig.ctx, realm, "svc-pending", "q").await;
    assert_eq!(status, STATUS_OK);
    assert!(String::from_utf8_lossy(&body).ends_with(":q"));
    rig.finish().await;
    drop(rig);
}

/// E1.5: unknown realm and child handles fail deterministically; a retired
/// handle is unknown the second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_realm_and_child_are_rejected() {
    let rig = boot().await;
    let (status, reply) =
        call_spawner(&rig.ctx, OP_SPAWN, json!({ "path": "child", "realm": 999 })).await;
    assert_eq!(status, STATUS_FAILED);
    assert_eq!(parse(&reply)["code"], "unknown_realm");
    let (status, reply) = realm_require(&rig.ctx, 999, "svc-child", "q").await;
    assert_eq!(status, STATUS_FAILED);
    assert_eq!(parse(&reply)["code"], "unknown_realm");
    let (status, _) = call_spawner(&rig.ctx, OP_RETIRE, json!({ "child": 42 })).await;
    assert_eq!(status, STATUS_FAILED);
    let reply = spawn_ok(&rig.ctx, 0, "child").await;
    let child = reply["child"].as_u64().expect("child");
    let (status, _) = call_spawner(&rig.ctx, OP_RETIRE, json!({ "child": child })).await;
    assert_eq!(status, STATUS_OK);
    let (status, _) = call_spawner(&rig.ctx, OP_RETIRE, json!({ "child": child })).await;
    assert_eq!(status, STATUS_FAILED, "a retired handle is unknown");
    rig.finish().await;
    drop(rig);
}

/// E1.6 + E1.14: unloadable and stale artifacts fail with the load detail and
/// leave no residue (the next success still gets child handle 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_failures_leave_no_residue() {
    let rig = boot().await;
    let (status, reply) = call_spawner(
        &rig.ctx,
        OP_SPAWN,
        json!({ "path": "/nonexistent/libnothing.so", "realm": 0 }),
    )
    .await;
    assert_eq!(status, STATUS_FAILED);
    assert_eq!(parse(&reply)["code"], "realm_spawn_load_failed");
    let (status, reply) =
        call_spawner(&rig.ctx, OP_SPAWN, json!({ "path": "stale", "realm": 0 })).await;
    assert_eq!(status, STATUS_FAILED);
    let detail = parse(&reply);
    assert_eq!(detail["code"], "realm_spawn_load_failed");
    assert!(
        detail["message"]
            .as_str()
            .is_some_and(|message| message.contains("ABI version mismatch")),
        "the message names both versions: {detail}"
    );
    assert_eq!(spawn_ok(&rig.ctx, 0, "child").await["child"], 1);
    rig.finish().await;
    drop(rig);
}

/// E1.7: the spawner's teardown disposes its children first (Eq. 31), and the
/// spawn links die with the activations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawner_teardown_cascades_to_children_first() {
    let rig = boot().await;
    spawn_ok(&rig.ctx, 0, "child").await;
    spawn_ok(&rig.ctx, 0, "child").await;
    assert_eq!(links_snapshot().len(), 2);
    rig.finish().await;
    let log = LOG.lock().expect("teardown log").clone();
    let links = links_snapshot();
    drop(rig);
    assert_eq!(
        log,
        ["child-teardown", "child-teardown", "spawner-teardown"]
    );
    assert!(links.is_empty(), "links die with the activations");
}

/// E1.8: retiring one child leaves its siblings serving and the retired
/// realm unresolvable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retire_removes_only_the_named_child() {
    let rig = boot().await;
    let first = spawn_ok(&rig.ctx, 0, "child").await;
    let second = spawn_ok(&rig.ctx, 0, "child").await;
    let (first_realm, second_realm) = (
        first["realm"].as_u64().expect("realm"),
        second["realm"].as_u64().expect("realm"),
    );
    let (status, _) = call_spawner(
        &rig.ctx,
        OP_RETIRE,
        json!({ "child": first["child"].as_u64().expect("child") }),
    )
    .await;
    assert_eq!(status, STATUS_OK);
    assert_eq!(
        links_snapshot().len(),
        1,
        "the retired child's link is gone"
    );
    let (status, _) = realm_require(&rig.ctx, first_realm, "svc-child", "q").await;
    assert_eq!(status, STATUS_INACTIVE);
    let (status, _) = realm_require(&rig.ctx, second_realm, "svc-child", "q").await;
    assert_eq!(status, STATUS_OK);
    rig.finish().await;
    drop(rig);
}

/// E1.9: spawn and retire are rejected inside a foreign entry and accepted on
/// a plugin-owned thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_retire_reject_foreign_entry_frames() {
    let rig = boot().await;
    let (status, reply) = call_spawner(
        &rig.ctx,
        OP_SPAWN_IN_ENTRY,
        json!({ "path": "child", "realm": 0 }),
    )
    .await;
    assert_eq!(status, STATUS_REENTRANCY);
    assert_eq!(parse(&reply)["code"], "reentrancy_cycle");
    let (status, _) = call_spawner(&rig.ctx, OP_RETIRE_IN_ENTRY, json!({ "child": 1 })).await;
    assert_eq!(status, STATUS_REENTRANCY);
    assert_eq!(spawn_ok(&rig.ctx, 0, "child").await["child"], 1);
    rig.finish().await;
    drop(rig);
}

/// E1.10: a child whose dependency is not committed settles as `pending`,
/// then activates once the dependency appears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_child_activates_when_its_dependency_arrives() {
    let rig = boot().await;
    let reply = spawn_ok(&rig.ctx, 0, "pending").await;
    assert_eq!(reply["state"], "pending");
    let realm = reply["realm"].as_u64().expect("realm");
    let (status, _) = realm_require(&rig.ctx, realm, "svc-pending", "q").await;
    assert_eq!(
        status, STATUS_INACTIVE,
        "a pending child provides nothing yet"
    );
    let _dependency = start(&rig.ctx, &CHILD_TABLE).await;
    let mut served = false;
    for _ in 0..200 {
        let (status, _) = realm_require(&rig.ctx, realm, "svc-pending", "q").await;
        if status == STATUS_OK {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(served, "the pending child converged after its dependency");
    rig.finish().await;
    drop(rig);
}

/// E1.11: a child whose setup fails is disposed and reported with the
/// startup error; no handle is consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_child_is_reported_and_disposed() {
    let rig = boot().await;
    let (status, reply) =
        call_spawner(&rig.ctx, OP_SPAWN, json!({ "path": "failing", "realm": 0 })).await;
    assert_eq!(status, STATUS_FAILED);
    assert_eq!(parse(&reply)["code"], "setup_failed");
    assert!(
        links_snapshot().is_empty(),
        "the failed child leaves no link"
    );
    assert_eq!(spawn_ok(&rig.ctx, 0, "child").await["child"], 1);
    rig.finish().await;
    drop(rig);
}

/// E1.12: teardown racing an in-flight `realm_require` stream settles within
/// the budget without a deadlock, children first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_races_in_flight_realm_requires() {
    let rig = boot().await;
    let reply = spawn_ok(&rig.ctx, 0, "child").await;
    let realm = reply["realm"].as_u64().expect("realm");
    let (status, _) = call_spawner(
        &rig.ctx,
        OP_HAMMER,
        json!({ "realm": realm, "service": "svc-child" }),
    )
    .await;
    assert_eq!(status, STATUS_OK);
    // Wait until the stream actually issued at least one require: sleeping a
    // fixed amount both flakes under load and can pass without any traffic.
    let mut warmed = false;
    for _ in 0..500 {
        if HAMMER_LAST.load(Ordering::SeqCst) != i32::MIN {
            warmed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(warmed, "the hammer stream issued its first require");
    rig.finish().await;
    let log = LOG.lock().expect("teardown log").clone();
    let last = HAMMER_LAST.load(Ordering::SeqCst);
    drop(rig);
    assert_ne!(last, i32::MIN, "the stream ran");
    // Whether the stream observed the gate is racy by design: the teardown
    // export sets `stop`, and a worker between iterations exits without
    // re-issuing. The deterministic gate coverage lives in the retire test
    // (INACTIVE after disposal) and the stale-invoker pins in
    // `outbound_tests`.
    let _ = last;
    assert_eq!(log, ["child-teardown", "spawner-teardown"]);
}

/// E1.13: many plugin threads spawn, call, and retire concurrently without
/// deadlock or lost handles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_spawn_call_retire_rounds_all_succeed() {
    let rig = boot().await;
    let (status, reply) =
        call_spawner(&rig.ctx, OP_CONCURRENT, json!({ "n": 8, "path": "child" })).await;
    assert_eq!(status, STATUS_OK);
    assert_eq!(String::from_utf8_lossy(&reply), "8");
    assert!(links_snapshot().is_empty(), "every round retired its child");
    rig.finish().await;
    drop(rig);
}

/// A reply buffer below the documented minimum is refused before any side
/// effect, so a retry never spawns twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_reply_buffer_is_refused_before_spawning() {
    let rig = boot().await;
    let (status, _) = call_spawner(
        &rig.ctx,
        OP_SPAWN,
        json!({ "path": "child", "realm": 0, "cap": REALM_SPAWN_REPLY_MIN - 1 }),
    )
    .await;
    assert_eq!(status, STATUS_CAPACITY);
    assert!(links_snapshot().is_empty(), "nothing was spawned");
    assert_eq!(spawn_ok(&rig.ctx, 0, "child").await["child"], 1);
    rig.finish().await;
    drop(rig);
}

/// B1 join arm: `realm_spawn` with a non-zero realm the activation opened
/// places the second child in the SAME realm; services of both children are
/// reachable; a same-name duplicate provide in one realm fails the child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_joins_an_existing_realm() {
    let rig = boot().await;
    let first = spawn_ok(&rig.ctx, 0, "child").await;
    let realm = first["realm"].as_u64().expect("realm");
    let (status, reply) = call_spawner(
        &rig.ctx,
        OP_SPAWN,
        json!({ "path": "child2", "realm": realm }),
    )
    .await;
    assert_eq!(status, STATUS_OK, "{}", String::from_utf8_lossy(&reply));
    let second = parse(&reply);
    assert_eq!(
        second["realm"].as_u64(),
        Some(realm),
        "joined the same realm"
    );
    assert_eq!(second["child"].as_u64(), Some(2));
    let (status, _) = realm_require(&rig.ctx, realm, "svc-child", "q").await;
    assert_eq!(status, STATUS_OK);
    let (status, _) = realm_require(&rig.ctx, realm, "svc-child2", "q").await;
    assert_eq!(status, STATUS_OK);
    // A same-name duplicate provide inside one realm fails the child
    // (kernel duplicate-provide), reported with the startup error.
    let (status, reply) = call_spawner(
        &rig.ctx,
        OP_SPAWN,
        json!({ "path": "child", "realm": realm }),
    )
    .await;
    assert_eq!(status, STATUS_FAILED);
    let detail = parse(&reply);
    assert_eq!(detail["code"], "duplicate_provide");
    rig.finish().await;
    drop(rig);
}

/// Shadowing (documented): when the spawner's own descriptor injects the
/// same name a realm child provides, the fiber's committed view wins the
/// dynamic read and the call resolves the injected (global) provider — the
/// authorization refuses it, fail-closed as `STATUS_UNDECLARED`. Spawners
/// must not inject the names their children provide.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawner_inject_shadows_the_realm_child_fail_closed() {
    let serial = SERIAL.lock().await;
    register_test_source("child", &CHILD_TABLE);
    LOG.lock().expect("teardown log").clear();
    let ctx = Context::new();
    // The global provider satisfies the shadowing spawner's inject.
    let _global = start(&ctx, &CHILD_TABLE).await;
    let shadowing: Arc<dyn Plugin> =
        Arc::new(from_static(&SHADOWING_TABLE).expect("shadowing table"));
    let spawner = ctx
        .plugin(shadowing, None)
        .expect("spawn shadowing spawner");
    tokio::time::timeout(Duration::from_secs(5), spawner.join())
        .await
        .expect("shadowing spawner settles")
        .expect("shadowing spawner ok");
    let reply = {
        let invoker = ctx
            .get::<Invoker>("svc-shadow")
            .expect("shadowing spawner committed");
        let body = json!({ "path": "child", "realm": 0 }).to_string();
        let bytes = tokio::time::timeout(
            Duration::from_secs(30),
            invoker.call(OP_SPAWN, body.into_bytes()),
        )
        .await
        .expect("no deadlock")
        .expect("call ok");
        let (status, reply) = unpack(&bytes);
        assert_eq!(status, STATUS_OK, "spawn ok");
        reply.to_vec()
    };
    let realm = parse(&reply)["realm"].as_u64().expect("realm");
    let body = json!({ "realm": realm, "service": "svc-child", "body": "q" }).to_string();
    let invoker = ctx
        .get::<Invoker>("svc-shadow")
        .expect("svc-shadow committed");
    let bytes = tokio::time::timeout(
        Duration::from_secs(30),
        invoker.call(OP_REQUIRE, body.into_bytes()),
    )
    .await
    .expect("no deadlock")
    .expect("call ok");
    let (status, detail) = unpack(&bytes);
    assert_eq!(
        status, STATUS_UNDECLARED,
        "fail-closed on the injected provider"
    );
    assert_eq!(parse(detail)["code"], "undeclared_dependency");
    tokio::time::timeout(Duration::from_secs(10), spawner.dispose())
        .await
        .expect("shadowing spawner tears down");
    drop(serial);
}

/// E1.12 spawn arm: a spawn racing the spawner's teardown is rejected
/// (the kernel refuses to start a child on an unloading parent; the gate
/// may also have flipped) instead of parking or leaking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_racing_teardown_fails_closed() {
    let rig = boot().await;
    spawn_ok(&rig.ctx, 0, "child").await;
    let handle = rig.spawner.clone();
    let dispose = tokio::spawn(async move { handle.dispose().await });
    let mut rejected = false;
    for _ in 0..50 {
        // Once teardown starts the service entry itself can fail closed
        // (`provider_gone` surfaces as an Err from the dynamic channel), and
        // a spawn that outran the dispose still succeeds while the spawner
        // is alive — both are legal interleavings.
        let invoker = rig
            .ctx
            .get::<Invoker>("svc-spawner")
            .expect("spawner committed");
        let attempt = tokio::time::timeout(
            Duration::from_secs(30),
            invoker.call(
                OP_SPAWN,
                json!({ "path": "child", "realm": 0 })
                    .to_string()
                    .into_bytes(),
            ),
        )
        .await
        .expect("no deadlock");
        // The teardown gate firing under the call surfaces as an Err from
        // the dynamic channel; both arms count as the spawn attempt.
        let attempt = attempt.map_or_else(
            |_| (STATUS_FAILED, Vec::new()),
            |bytes| {
                let (status, reply) = unpack(&bytes);
                (status, reply.to_vec())
            },
        );
        if attempt.0 == STATUS_FAILED {
            rejected = true;
            let _ = parse(&attempt.1);
            break;
        }
        assert_eq!(attempt.0, STATUS_OK, "spawn while alive must succeed");
    }
    assert!(rejected, "a spawn landed after teardown started and failed");
    dispose.await.expect("dispose task");
    drop(rig);
}

/// C2 for setup-time spawns: a child spawned from a plugin-owned thread
/// started inside the spawner's `setup` is disposed BEFORE the spawner's
/// teardown export runs (its cascade effect registered after
/// `dylib.teardown`, so LIFO drains it first).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_time_spawn_tears_down_before_the_teardown_export() {
    let serial = SERIAL.lock().await;
    register_test_source("child", &CHILD_TABLE);
    LOG.lock().expect("teardown log").clear();
    SETUP_SPAWN_RESULT
        .lock()
        .expect("setup spawn result")
        .take();
    let ctx = Context::new();
    let spawner: Arc<dyn Plugin> =
        Arc::new(from_static(&SETUP_SPAWNER_TABLE).expect("setup spawner table"));
    let handle = ctx.plugin(spawner, None).expect("spawn setup spawner");
    tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("setup spawner settles")
        .expect("setup spawner ok");
    let mut issued = false;
    for _ in 0..500 {
        if SETUP_SPAWN_RESULT
            .lock()
            .expect("setup spawn result")
            .is_some()
        {
            issued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(issued, "the setup thread issued its spawn");
    let (status, reply) = SETUP_SPAWN_RESULT
        .lock()
        .expect("setup spawn result")
        .take()
        .expect("spawn recorded");
    assert_eq!(status, STATUS_OK, "{}", String::from_utf8_lossy(&reply));
    assert_eq!(links_snapshot().len(), 1);
    tokio::time::timeout(Duration::from_secs(10), handle.dispose())
        .await
        .expect("setup spawner tears down");
    let log = LOG.lock().expect("teardown log").clone();
    let links = links_snapshot();
    drop(serial);
    assert_eq!(log, ["child-teardown", "setup-spawner-teardown"]);
    assert!(links.is_empty(), "links die with the activations");
}
