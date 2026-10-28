// Rust guideline compliant 2026-10-24
//! Teardown fault injection for the dylib adapter (S7): a teardown that
//! hangs is orphaned by the cleanup budget while the rest of the unload
//! path completes, and teardown runs exactly once even when dispose calls
//! race. A teardown that *panics* is outside the testable surface by ABI
//! contract: plugin exports are `extern "C"` (nounwind), so a foreign
//! panic aborts the process — dylib is trusted code (D5).
//!
//! Each test owns its statics outright (`HANG_*` / `ONCE_*`), so the two
//! fixtures never interfere under parallel test threads.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use dougong::{Context, Plugin, TimeoutConfig};
use dougong_abi::{ABI_VERSION, HostExports, PluginExports, STATUS_CAPACITY, STATUS_OK, write_out};

use super::from_static;

/// Test rail: any hang becomes a failure instead of a stuck suite.
const RAIL: Duration = Duration::from_secs(5);

/// Small real-time budget: the hung side never completes on its own, so a
/// short real duration is deterministic under load (S3 test convention).
const BUDGET: Duration = Duration::from_millis(50);

/// Polls the condition with a hard ceiling: a hang fails, never flakes.
async fn wait_until(condition: impl Fn() -> bool + Send + Sync) {
    tokio::time::timeout(RAIL, async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition reached");
}

unsafe extern "C" fn fault_name(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"fault-plugin", out, cap, len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn fault_descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe {
        write_out(
            br#"{"name":"fault-plugin","provide":["fault.svc"],"inject":[]}"#,
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

unsafe extern "C" fn fault_setup(_host: *const HostExports) -> *mut std::os::raw::c_void {
    // One live byte per activation; the teardowns release it.
    Box::into_raw(Box::new(0_u8)).cast::<std::os::raw::c_void>()
}

unsafe extern "C" fn fault_call(
    _handle: *mut std::os::raw::c_void,
    _method: u32,
    _data: *const u8,
    _len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: host buffer contract.
    if unsafe { write_out(b"echo", out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn fault_on_event(
    _handle: *mut std::os::raw::c_void,
    _event: u32,
    _mode: u32,
    _data: *const u8,
    _len: usize,
    _next_available: u32,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: host buffer contract (never dispatched: the fixture declares
    // no listeners).
    if unsafe { write_out(b"null", out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

unsafe extern "C" fn fault_on_next(
    _handle: *mut std::os::raw::c_void,
    _event: u32,
    _chain_status: i32,
    _data: *const u8,
    _len: usize,
    out: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: host buffer contract (see `fault_on_event`).
    if unsafe { write_out(b"null", out, cap, out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

// ---------------------------------------------------------------------------
// Hung-teardown fixture (cleanup-budget orphan arm)
// ---------------------------------------------------------------------------

/// Release side of the teardown gate, armed by the test.
static HANG_RELEASE: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();
/// Wait side of the teardown gate; the teardown blocks on it.
static HANG_WAIT: OnceLock<Mutex<std::sync::mpsc::Receiver<()>>> = OnceLock::new();
/// Number of times the teardown was entered.
static HANG_ENTERED: AtomicUsize = AtomicUsize::new(0);
/// Number of times the teardown returned.
static HANG_RETURNED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn hanging_teardown(handle: *mut std::os::raw::c_void) {
    HANG_ENTERED.fetch_add(1, Ordering::SeqCst);
    if !handle.is_null() {
        // SAFETY: the handle came from `fault_setup` and is torn down once.
        drop(unsafe { Box::from_raw(handle.cast::<u8>()) });
    }
    let receiver = HANG_WAIT.get().expect("gate armed by the test");
    // Block inside the foreign call: the host must orphan this disposer
    // under the cleanup budget instead of letting it park the unload.
    receiver
        .lock()
        .expect("gate lock")
        .recv()
        .expect("release channel open");
    HANG_RETURNED.fetch_add(1, Ordering::SeqCst);
}

static HANG_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: fault_name,
    descriptor: fault_descriptor,
    setup: fault_setup,
    call: fault_call,
    on_event: fault_on_event,
    on_next: fault_on_next,
    teardown: hanging_teardown,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_teardown_is_orphaned_and_the_unload_completes() {
    super::arm_deadlock_watchdog();
    let (release_tx, wait_rx) = std::sync::mpsc::channel();
    let _ = HANG_RELEASE.set(release_tx);
    let _ = HANG_WAIT.set(Mutex::new(wait_rx));

    let ctx = Context::new_with_timeouts(TimeoutConfig {
        cleanup: Some(BUDGET),
        ..TimeoutConfig::default()
    });
    let plugin: Arc<dyn Plugin> = Arc::new(from_static(&HANG_TABLE).expect("hang table"));
    let handle = ctx.plugin(plugin, None).expect("spawn fixture");
    tokio::time::timeout(RAIL, handle.join())
        .await
        .expect("activation settles")
        .expect("activation ok");

    // The teardown hangs inside the foreign call; the cleanup budget must
    // orphan it (never abort it) so disposal still completes.
    tokio::time::timeout(RAIL, handle.dispose())
        .await
        .expect("dispose is not parked by the hung teardown");
    assert_eq!(handle.state(), dougong::FiberState::Disposed);
    assert_eq!(
        HANG_ENTERED.load(Ordering::SeqCst),
        1,
        "teardown was entered"
    );
    assert_eq!(
        HANG_RETURNED.load(Ordering::SeqCst),
        0,
        "still blocked inside"
    );
    // The provide disposer runs before the teardown in LIFO: the service
    // is withdrawn even though the teardown never returned.
    assert!(ctx.get::<dougong::Invoker>("fault.svc").is_none());

    // The orphaned disposer keeps running in the background and finishes
    // once the foreign call returns.
    HANG_RELEASE
        .get()
        .expect("gate armed")
        .send(())
        .expect("release the teardown");
    wait_until(|| HANG_RETURNED.load(Ordering::SeqCst) == 1).await;
    ctx.stop().await;
}

// ---------------------------------------------------------------------------
// Exactly-once-teardown fixture (concurrent dispose arm)
// ---------------------------------------------------------------------------

/// Number of times the teardown ran.
static ONCE_TEARDOWNS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn counting_teardown(handle: *mut std::os::raw::c_void) {
    ONCE_TEARDOWNS.fetch_add(1, Ordering::SeqCst);
    if !handle.is_null() {
        // SAFETY: the handle came from `fault_setup` and is torn down once.
        drop(unsafe { Box::from_raw(handle.cast::<u8>()) });
    }
}

static ONCE_TABLE: PluginExports = PluginExports {
    abi_version: ABI_VERSION,
    name: fault_name,
    descriptor: fault_descriptor,
    setup: fault_setup,
    call: fault_call,
    on_event: fault_on_event,
    on_next: fault_on_next,
    teardown: counting_teardown,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_runs_exactly_once_under_concurrent_dispose() {
    super::arm_deadlock_watchdog();
    let ctx = Context::new();
    let plugin: Arc<dyn Plugin> = Arc::new(from_static(&ONCE_TABLE).expect("once table"));
    let handle = Arc::new(ctx.plugin(plugin, None).expect("spawn fixture"));
    tokio::time::timeout(RAIL, handle.join())
        .await
        .expect("activation settles")
        .expect("activation ok");

    // Four racing dispose calls: the S4 single-winner guard must let
    // exactly one teardown through the adapter.
    let mut callers = Vec::new();
    for _ in 0..4 {
        let handle = Arc::clone(&handle);
        callers.push(tokio::spawn(async move { handle.dispose().await }));
    }
    for caller in callers {
        tokio::time::timeout(RAIL, caller)
            .await
            .expect("dispose settles")
            .expect("dispose task");
    }

    assert_eq!(handle.state(), dougong::FiberState::Disposed);
    assert_eq!(
        ONCE_TEARDOWNS.load(Ordering::SeqCst),
        1,
        "teardown ran once"
    );
    ctx.stop().await;
}
