// Rust guideline compliant 2026-11-05
//! Outbound realm activation (ABI v6, design decision D33,
//! `doc/maintenance/realm_requirements.md`).
//!
//! `realm_spawn` activates a child plugin artifact inside a realm the calling
//! activation owns; the child's fiber is registered as an effect of the
//! caller's fiber (paper Eq. 31), so the caller's teardown cascades to every
//! child before the caller's own `teardown` export runs. `realm_retire`
//! disposes one child early. `realm_require` calls a service provided by one
//! of the caller's own children: the realm is resolved with the kernel's
//! realm-first-then-global rule (D15 — completed for dynamic reads in D33 §9)
//! and the call is authorized only when the resolved provider's activation id
//! is linked to the caller in the spawn-link table, the same "bind the
//! resolved provider" discipline D31 grants use. The bridge composes
//! `Context::isolate_realm`, `Context::plugin`, `Context::uninstall`,
//! `FiberHandle::join`, and `FiberHandle::dispose`; its tests resolve child
//! paths to embedded export tables instead of `dlopen`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use dougong::{FiberHandle, FiberState, Invoker, Plugin};
use dougong_abi::{
    REALM_SPAWN_REPLY_MIN, STATUS_CAPACITY, STATUS_FAILED, STATUS_HOST_ERROR, STATUS_INACTIVE,
    STATUS_OK, STATUS_REENTRANCY, STATUS_UNDECLARED, write_out,
};
use parking_lot::Mutex;
use serde_json::json;

use super::{
    Activation, DynPlugin, ENTRY_STACK, Exports, ReplyBuffer, drive, from_exports, input_slice,
    open_exports, provider_gone_detail, settle_require, undeclared_require, write_detail,
};

/// Realm labels and child handles one activation opened (see
/// [`Activation::realms`]). Handles and labels are meaningful only to the
/// activation that received them, which is what keeps the capability narrow.
#[derive(Default)]
pub struct RealmState {
    /// Labels returned by this activation's `realm_spawn` calls. Entries are
    /// kept for the activation's lifetime (bounds the capability surface the
    /// activation may keep addressing; each entry is one u64).
    labels: HashSet<u64>,
    /// Live children by handle, with the plugin object so an early dispose
    /// can also reclaim the child's registry runtime record (the Eq. 31
    /// cascade disposes the fiber but never touches the registry).
    children: HashMap<u64, (FiberHandle, Arc<dyn Plugin>)>,
    /// Last child handle issued; handles start at 1 so `0` is never valid.
    last_child: u64,
}

/// Process-wide spawn links: child activation id -> spawning activation id.
/// Instance ids are process-unique, so independent kernel roots cannot alias.
/// A process that never spawns keeps the map empty and never locks it on any
/// other path.
static SPAWN_LINKS: LazyLock<Mutex<HashMap<u64, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Records `child` as spawned by `parent`; called before the child's `setup`
/// so the link precedes every `provide` the child can commit (D33 C5).
pub fn link_child(child: u64, parent: u64) {
    SPAWN_LINKS.lock().insert(child, parent);
}

/// Removes the link of a reclaimed activation; a no-op for activations the
/// loader or kernel started.
pub fn unlink_child(activation: &Activation) {
    if activation.parent.is_some() {
        SPAWN_LINKS.lock().remove(&activation.instance_id);
    }
}

/// Every live link as `(child, parent)`; tests assert the table drains.
#[cfg(test)]
pub fn links_snapshot() -> Vec<(u64, u64)> {
    SPAWN_LINKS
        .lock()
        .iter()
        .map(|(child, parent)| (*child, *parent))
        .collect()
}

/// Disposes and unregisters every child still tracked by `activation`. The
/// Eq. 31 cascade has already disposed the fibers by the time the
/// activation's teardown arm runs; this reclaims the registry runtime
/// records (idempotent) so a long-lived process does not accumulate one
/// `Runtime` per spawn.
pub async fn teardown_realms(activation: &Activation) {
    let children: Vec<(FiberHandle, Arc<dyn Plugin>)> = activation
        .realms
        .lock()
        .children
        .drain()
        .map(|(_, pair)| pair)
        .collect();
    for (handle, plugin) in children {
        handle.dispose().await;
        activation.ctx.uninstall(&plugin).await;
    }
}

/// True when `child` was spawned by `parent` and is still linked.
fn is_child_of(child: u64, parent: u64) -> bool {
    SPAWN_LINKS.lock().get(&child) == Some(&parent)
}

/// Artifact paths the tests resolve to embedded static tables instead of a
/// real shared library (the same device `from_static` serves for hosts that
/// embed tables).
#[cfg(test)]
static TEST_SOURCES: LazyLock<Mutex<HashMap<String, &'static dougong_abi::PluginExports>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Binds a test artifact path to an embedded export table.
#[cfg(test)]
pub fn register_test_source(path: &str, table: &'static dougong_abi::PluginExports) {
    TEST_SOURCES.lock().insert(path.to_string(), table);
}

/// Resolves one `realm_spawn` artifact path into a child plugin linked to its
/// spawner.
fn resolve_child(path: &str, parent: u64) -> dougong::Result<DynPlugin> {
    let exports = child_exports(path)?;
    let mut plugin = from_exports(exports)?;
    plugin.spawned_by = Some(parent);
    Ok(plugin)
}

fn child_exports(path: &str) -> dougong::Result<Exports> {
    #[cfg(test)]
    if let Some(table) = TEST_SOURCES.lock().get(path).copied() {
        return Ok(Exports { table, _lib: None });
    }
    open_exports(Path::new(path))
}

/// One realm-entry failure: the detail triple plus the status the C entry
/// reports (the retire entry has no buffer and only logs it).
struct RealmFailure {
    code: String,
    class: &'static str,
    message: String,
    status: i32,
}

impl RealmFailure {
    fn logic(code: &str, message: impl Into<String>, status: i32) -> Self {
        Self {
            code: code.to_string(),
            class: dougong::ErrorClass::Logic.as_str(),
            message: message.into(),
            status,
        }
    }

    fn from_error(err: &dougong::Error) -> Self {
        Self {
            code: err.code.to_string(),
            class: err.class.as_str(),
            message: err.message.clone(),
            status: STATUS_FAILED,
        }
    }

    fn write(&self, reply: ReplyBuffer) -> i32 {
        write_detail(
            reply.out_buf,
            reply.cap,
            reply.out_len,
            &self.code,
            self.class,
            &self.message,
            self.status,
        )
    }

    /// Writes the detail guaranteed to fit `reply.cap` (which the caller
    /// checked is at least [`REALM_SPAWN_REPLY_MIN`]): the full message
    /// first, then a truncated one, then the bare status. Spawn failures run
    /// after real side effects, so a `STATUS_CAPACITY` here must never
    /// happen — the caller could otherwise retry and re-run the child's
    /// setup.
    fn write_fitting(self, reply: ReplyBuffer) -> i32 {
        let written = self.write(reply);
        if written != STATUS_CAPACITY {
            return written;
        }
        let mut shortened = self;
        for budget in [64_usize, 16] {
            shortened.message = truncate_to(&shortened.message, budget);
            let written = shortened.write(reply);
            if written != STATUS_CAPACITY {
                return written;
            }
        }
        shortened.status
    }
}

/// Truncates `message` (on a char boundary) to a JSON detail that fits the
/// worst-case envelope within `budget` bytes for the message field.
fn truncate_to(message: &str, budget: usize) -> String {
    let mut cut = message
        .char_indices()
        .take_while(|(index, _)| index + 3 <= budget)
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    if cut < message.len() {
        cut = cut.saturating_sub(3);
        while cut > 0 && !message.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &message[..cut])
    } else {
        message.to_string()
    }
}

/// Posture shared by the spawn and retire entries: a foreign entry frame on
/// the calling thread holds that instance's lock, so a child requiring the
/// spawner during setup (or a child's teardown joining the lock) would wait
/// on a lock the wait-cycle detector cannot see (the D32 B6 argument); and a
/// flipped liveness gate means the activation is already tearing down.
fn posture_failure(activation: &Activation) -> Option<RealmFailure> {
    let foreign_entry = ENTRY_STACK.with(|stack| !stack.borrow().is_empty());
    if foreign_entry {
        return Some(RealmFailure::logic(
            "reentrancy_cycle",
            "realm spawn and retire must originate on a plugin-owned thread: a foreign \
             entry is executing on the calling thread",
            STATUS_REENTRANCY,
        ));
    }
    if !activation.alive.load(Ordering::Acquire) {
        let (code, message) = provider_gone_detail();
        return Some(RealmFailure::logic(code, message, STATUS_FAILED));
    }
    None
}

fn audit(activation: &Activation, level: dougong::LogLevel, message: String, cause: Option<&str>) {
    let record = dougong::LogRecord::new(level, "dylib", "outbound_realm", message)
        .with_trace(Some(activation.ctx.trace_id()))
        .with_boundary(Some("dylib"));
    let record = match cause {
        Some(cause) => record.with_cause(cause),
        None => record,
    };
    activation.ctx.logger().log_record(&record);
}

/// The `realm_spawn` bridge (D33 B1-B6).
fn spawn_impl(activation: &Activation, realm: u64, path: &[u8], reply: ReplyBuffer) -> i32 {
    // Capacity is judged before any side effect, so a retry after
    // `STATUS_CAPACITY` can never spawn twice.
    if reply.cap < REALM_SPAWN_REPLY_MIN {
        return STATUS_CAPACITY;
    }
    if let Some(failure) = posture_failure(activation) {
        return failure.write(reply);
    }
    let label = if realm == 0 {
        activation.ctx.fresh_label()
    } else if activation.realms.lock().labels.contains(&realm) {
        realm
    } else {
        return RealmFailure::logic(
            "unknown_realm",
            format!("realm {realm} was not opened by this activation"),
            STATUS_FAILED,
        )
        .write(reply);
    };
    let Ok(path) = std::str::from_utf8(path) else {
        return RealmFailure::logic(
            "realm_spawn_load_failed",
            "artifact path must be valid UTF-8",
            STATUS_FAILED,
        )
        .write(reply);
    };
    let plugin: Arc<dyn Plugin> = match resolve_child(path, activation.instance_id) {
        Ok(plugin) => Arc::new(plugin),
        Err(err) => {
            return RealmFailure {
                code: "realm_spawn_load_failed".to_string(),
                class: err.class.as_str(),
                message: err.message,
                status: STATUS_FAILED,
            }
            .write(reply);
        }
    };
    // The child fiber is an effect of the caller's fiber (Eq. 31): the realm
    // context shares the caller's fiber and only swaps the scope.
    let realm_ctx = activation.ctx.isolate_realm(label);
    let settled = drive(&activation.rt, {
        let plugin = Arc::clone(&plugin);
        async move {
            let handle = realm_ctx.plugin(Arc::clone(&plugin), None)?;
            let joined = handle.join().await;
            Ok::<_, dougong::Error>((handle, joined, plugin))
        }
    });
    let (handle, joined, plugin) = match settled {
        Ok(triple) => triple,
        Err(err) => {
            // `plugin()` itself failed (e.g. the caller is unloading): no
            // fiber was spawned, but the registry runtime record exists and
            // must go.
            drive(&activation.rt, activation.ctx.uninstall(&plugin));
            return RealmFailure::from_error(&err).write(reply);
        }
    };
    let state = match (joined, handle.state()) {
        (Ok(()), FiberState::Active) => "active",
        (Ok(()), FiberState::Pending) => "pending",
        (joined, _) => {
            // Failed or not settled: dispose the fiber and reclaim the
            // registry record so the failure leaves no residue.
            drive(&activation.rt, async {
                handle.dispose().await;
                activation.ctx.uninstall(&plugin).await;
            });
            let failure = joined.err().map_or_else(
                || {
                    RealmFailure::logic(
                        "realm_spawn_not_settled",
                        "child activation did not settle active or pending",
                        STATUS_FAILED,
                    )
                },
                |err| RealmFailure::from_error(&err),
            );
            return failure.write_fitting(reply);
        }
    };
    let child = {
        let mut realms = activation.realms.lock();
        realms.labels.insert(label);
        realms.last_child += 1;
        let child = realms.last_child;
        realms.children.insert(child, (handle, plugin));
        child
    };
    audit(
        activation,
        dougong::LogLevel::Info,
        format!("realm_spawn realm={label} child={child} state={state} ok"),
        None,
    );
    let body = json!({ "realm": label, "child": child, "state": state }).to_string();
    // SAFETY: the host honors the same caller-owned buffer contract it
    // requires of plugin exports; the capacity guard above makes the fixed
    // reply fit.
    if unsafe { write_out(body.as_bytes(), reply.out_buf, reply.cap, reply.out_len) } {
        STATUS_OK
    } else {
        STATUS_CAPACITY
    }
}

/// The `realm_retire` bridge (D33 C1).
fn retire_impl(activation: &Activation, child: u64) -> Result<(), RealmFailure> {
    if let Some(failure) = posture_failure(activation) {
        return Err(failure);
    }
    let Some((handle, plugin)) = activation.realms.lock().children.remove(&child) else {
        return Err(RealmFailure::logic(
            "unknown_child",
            format!("child {child} is not a live child of this activation"),
            STATUS_FAILED,
        ));
    };
    // Dispose the fiber, then reclaim the registry runtime record (the Eq. 31
    // cascade covers the fiber only).
    drive(&activation.rt, async {
        handle.dispose().await;
        activation.ctx.uninstall(&plugin).await;
    });
    audit(
        activation,
        dougong::LogLevel::Info,
        format!("realm_retire child={child} ok"),
        None,
    );
    Ok(())
}

/// The `realm_require` bridge (D33 C3/C4).
#[expect(
    clippy::too_many_arguments,
    reason = "the vtable signature is fixed by the ABI contract"
)]
fn realm_require_impl(
    activation: &Activation,
    realm: u64,
    service: *const u8,
    service_len: usize,
    method: u32,
    in_data: *const u8,
    in_len: usize,
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: vtable input contract (null pairs with length 0).
    let service_bytes = unsafe { input_slice(service, service_len) };
    let Ok(service) = std::str::from_utf8(service_bytes) else {
        return write_detail(
            out_buf,
            cap,
            out_len,
            "undeclared_dependency",
            dougong::ErrorClass::Logic.as_str(),
            "service name is not valid UTF-8",
            STATUS_UNDECLARED,
        );
    };
    if !activation.realms.lock().labels.contains(&realm) {
        return write_detail(
            out_buf,
            cap,
            out_len,
            "unknown_realm",
            dougong::ErrorClass::Logic.as_str(),
            &format!("realm {realm} was not opened by this activation"),
            STATUS_FAILED,
        );
    }
    let Some(invoker) = activation.ctx.isolate_realm(realm).get::<Invoker>(service) else {
        return write_detail(
            out_buf,
            cap,
            out_len,
            "inactive_dependency",
            dougong::ErrorClass::Logic.as_str(),
            &format!("cannot get required service \"{service}\" in inactive context"),
            STATUS_INACTIVE,
        );
    };
    // Authorization binds the resolved provider: a same-name provider in the
    // global realm, in another activation's realm, or one this activation did
    // not spawn stays unauthorized (the realm fallback to the global default
    // must never widen the capability).
    let authorized = invoker
        .inner
        .activation_id()
        .is_some_and(|provider| is_child_of(provider, activation.instance_id));
    if !authorized {
        return undeclared_require(service, out_buf, cap, out_len);
    }
    settle_require(
        activation, service, &invoker, method, in_data, in_len, out_buf, cap, out_len,
    )
}

/// vtable entry: activates a child plugin artifact in a realm.
///
/// # Safety
///
/// Per the [`dougong_abi::HostExports`] contract: `host` names a live
/// activation, `path` is readable for `path_len` bytes, and `out_buf`/
/// `out_len` follow the caller-owned buffer protocol.
pub unsafe extern "C" fn host_realm_spawn(
    host: *mut std::ffi::c_void,
    realm: u64,
    path: *const u8,
    path_len: usize,
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: the vtable contract keeps `host` valid until teardown returns.
    let activation = unsafe { &*host.cast::<Activation>() };
    let reply = ReplyBuffer {
        out_buf,
        cap,
        out_len,
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: vtable input contract (null pairs with length 0).
        let path = unsafe { input_slice(path, path_len) };
        spawn_impl(activation, realm, path, reply)
    }))
    .unwrap_or(STATUS_HOST_ERROR)
}

/// vtable entry: disposes one realm child.
///
/// # Safety
///
/// Per the [`dougong_abi::HostExports`] contract: `host` names a live
/// activation.
pub unsafe extern "C" fn host_realm_retire(host: *mut std::ffi::c_void, child: u64) -> i32 {
    // SAFETY: the vtable contract keeps `host` valid until teardown returns.
    let activation = unsafe { &*host.cast::<Activation>() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match retire_impl(activation, child) {
            Ok(()) => STATUS_OK,
            Err(failure) => {
                audit(
                    activation,
                    dougong::LogLevel::Error,
                    format!("realm_retire rejected: {}", failure.message),
                    Some(&failure.code),
                );
                failure.status
            }
        }
    }))
    .unwrap_or(STATUS_HOST_ERROR)
}

/// vtable entry: `require` addressed to a realm.
///
/// # Safety
///
/// Per the [`dougong_abi::HostExports`] contract: `host` names a live
/// activation, `service`/`in_data` are readable for their lengths, and
/// `out_buf`/`out_len` follow the caller-owned buffer protocol.
pub unsafe extern "C" fn host_realm_require(
    host: *mut std::ffi::c_void,
    realm: u64,
    service: *const u8,
    service_len: usize,
    method: u32,
    in_data: *const u8,
    in_len: usize,
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    // SAFETY: the vtable contract keeps `host` valid until teardown returns.
    let activation = unsafe { &*host.cast::<Activation>() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        realm_require_impl(
            activation,
            realm,
            service,
            service_len,
            method,
            in_data,
            in_len,
            out_buf,
            cap,
            out_len,
        )
    }))
    .unwrap_or_else(|_| {
        write_detail(
            out_buf,
            cap,
            out_len,
            "host_panic",
            dougong::ErrorClass::Transport.as_str(),
            "host realm_require entry panicked",
            STATUS_HOST_ERROR,
        )
    })
}
