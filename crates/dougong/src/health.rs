// Rust guideline compliant 2026-10-25
//! Health snapshot (observability track O5, improve 三.5).
//!
//! [`Context::health`](crate::Context::health) assembles the point-in-time
//! face a host health endpoint serves (Phase 0 ruling OBS: HTTP wiring
//! belongs to the host — `/healthz`, `/readyz`, and friends are compositions
//! over this snapshot). The kernel only collects.

use std::time::Duration;

use crate::fiber::FiberState;
use crate::metrics::RecentError;

/// One fiber parked in `Failed` with its startup error (O5).
#[derive(Debug, Clone)]
pub struct FailedFiber {
    /// Plugin display name.
    pub name: String,
    /// Fiber uid; `None` once disposed.
    pub uid: Option<u64>,
    /// The fiber's O3 trace id.
    pub trace: u64,
    /// The startup error text.
    pub error: String,
}

/// One fiber with a tracked (boundary) call in flight (O5). "How long is too
/// long" is the host's threshold to apply against [`LongTask::longest`].
#[derive(Debug, Clone)]
pub struct LongTask {
    /// Plugin display name.
    pub name: String,
    /// Fiber uid; `None` once disposed.
    pub uid: Option<u64>,
    /// The fiber's O3 trace id.
    pub trace: u64,
    /// Age of the longest-running in-flight tracked call.
    pub longest: Duration,
}

/// One `(service, label)` slot whose provider fiber is not `Active` (O5): a
/// held implementation nobody can strictly resolve right now.
///
/// By LIFO construction such windows are bounded to in-flight teardowns,
/// which is exactly the anomaly this face surfaces.
#[derive(Debug, Clone)]
pub struct StaleProvider {
    /// Service name of the held slot.
    pub service: String,
    /// Isolation label of the slot.
    pub label: u64,
    /// Provider fiber name, when the fiber is still registered.
    pub fiber: Option<String>,
    /// Provider uid the slot committed under.
    pub uid: u64,
    /// Provider fiber state at snapshot time; a slot whose fiber is already
    /// gone reports `Disposed`.
    pub state: FiberState,
}

/// Point-in-time runtime health (O5).
///
/// Counts mirror the O2 metrics face; the detail lists (failed fibers,
/// recent errors, long tasks, stale providers) are the additions that turn
/// counts into an actionable endpoint body.
#[derive(Debug, Clone, Default)]
pub struct HealthSnapshot {
    /// Distinct plugin runtimes in the registry.
    pub plugin_count: u64,
    /// Fibers currently in `Pending`.
    pub pending_fibers: u64,
    /// Fibers currently in `Loading`.
    pub loading_fibers: u64,
    /// Fibers currently `Active`.
    pub active_fibers: u64,
    /// Fibers currently `Failed`.
    pub failed_fibers: u64,
    /// Fibers currently `Unloading`.
    pub unloading_fibers: u64,
    /// Fibers in any state other than `Disposed`.
    pub live_fibers: u64,
    /// Failed fibers with their startup errors.
    pub failed: Vec<FailedFiber>,
    /// Graded error records, oldest first, capped (O5 ring).
    pub recent_errors: Vec<RecentError>,
    /// Fibers with tracked calls in flight.
    pub long_tasks: Vec<LongTask>,
    /// Held slots whose provider fiber is not `Active`.
    pub stale_providers: Vec<StaleProvider>,
    /// The default judgment: no failed fibers, no stale providers. Hosts
    /// may derive readiness differently (queue depth, error recency, ...).
    pub healthy: bool,
}
