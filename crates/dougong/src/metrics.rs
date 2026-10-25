// Rust guideline compliant 2026-10-25
//! Runtime metrics (observability track O2, improve 三.2).
//!
//! Two kinds of signals with two cost models. Counters are always-on
//! `Relaxed` atomics on cold paths — a state transition or a notify is
//! already a multi-hundred-nanosecond event, so one relaxed add is noise.
//! Latency instrumentation is opt-in through [`MetricsConfig`] because
//! `Instant::now()` is not free on the strict-resolution path, which runs
//! on every strict `get`; the default (`latencies: false`) keeps that path
//! untouched.
//!
//! The kernel only *collects*: hosts wire Prometheus/OpenTelemetry exporters
//! from [`MetricsSnapshot`] (Phase 0 ruling OBS = library-level API + host
//! wiring).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;

use crate::error::{ErrorCategory, ErrorClass};
use crate::logger::LogRecord;
use crate::policy::CategoryAlert;

/// How many graded error records the recent-errors ring (O5) retains. Errors
/// are a cold path (a few per anomaly at most), so a fixed 64 covers every
/// realistic inspection window without unbounded retention.
const RECENT_ERROR_CAP: usize = 64;

/// One graded error record, copied into the recent-errors ring at the S5
/// emit funnel (O5 health face; `trace` is the O3 fiber trace id).
#[derive(Debug, Clone)]
pub struct RecentError {
    /// Error class stamped by the policy table.
    pub class: ErrorClass,
    /// Alerting group (O6), stamped next to the class.
    pub category: Option<ErrorCategory>,
    /// Fiber / plugin name, when the record has one.
    pub fiber: Option<String>,
    /// Fiber uid (`None` also covers dispose-complete sites).
    pub uid: Option<u64>,
    /// The fiber's O3 trace id.
    pub trace: Option<u64>,
    /// Stable record kind (`fail_load`, `listener_failure`, ...).
    pub event: String,
    /// Human-readable summary, verbatim from the record.
    pub message: String,
    /// When the record was graded.
    pub at: SystemTime,
}

/// Upper bounds (in nanoseconds) of the first seven latency buckets; an
/// eighth bucket catches everything at or above one second. Log2-spaced so
/// "slow path" shows up without storing per-sample data.
const BUCKET_BOUNDS_NS: [u64; 7] = [
    1_000, // < 1 us
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];

/// Latency instrumentation switches (O2), fixed at root construction.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetricsConfig {
    /// Times the five instrumented waits (service resolution, event
    /// dispatch, plugin update, effect cleanup, provider drain). `false`
    /// (the default) records no latency samples at all.
    pub latencies: bool,
}

/// One latency metric: count, total, max, and a fixed log2 bucket
/// histogram. Never read while running — snapshots copy it out.
#[derive(Debug, Default)]
pub struct LatencyStat {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
    buckets: [AtomicU64; 8],
}

impl LatencyStat {
    #[must_use]
    pub fn default_stat() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub fn record(&self, sample: Duration) {
        let ns = u64::try_from(sample.as_nanos()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        // `position` cannot fail: bounds has 7 entries, so the fallback
        // bucket index 7 is reachable exactly when every bound is smaller.
        let index = BUCKET_BOUNDS_NS
            .iter()
            .position(|&bound| ns < bound)
            .unwrap_or(7);
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
    }

    pub fn summarize(&self) -> LatencySummary {
        let count = self.count.load(Ordering::Relaxed);
        let mut buckets = [0_u64; 8];
        for (target, bucket) in buckets.iter_mut().zip(&self.buckets) {
            *target = bucket.load(Ordering::Relaxed);
        }
        LatencySummary {
            count,
            total: Duration::from_nanos(self.total_ns.load(Ordering::Relaxed)),
            max: Duration::from_nanos(self.max_ns.load(Ordering::Relaxed)),
            buckets,
        }
    }
}

/// Kernel-resident metrics block (O2): always-on counters plus the opt-in
/// latency slots. All reads go through snapshots.
#[derive(Debug)]
pub struct Metrics {
    /// Whether latency slots record (construction-fixed).
    latencies: bool,
    pub state_transitions: AtomicU64,
    pub notifies: AtomicU64,
    pub apply_retries: AtomicU64,
    pub logic_errors: AtomicU64,
    pub timeout_errors: AtomicU64,
    pub transport_errors: AtomicU64,
    pub resolution: LatencyStat,
    pub update: LatencyStat,
    pub cleanup: LatencyStat,
    pub drain: LatencyStat,
    /// Recent-errors ring (O5), oldest first, capped at [`RECENT_ERROR_CAP`].
    recent: Mutex<VecDeque<RecentError>>,
    /// Per-category alert/rate-limit windows (O6), touched only from the
    /// emit funnel (a cold path); one slot per [`ErrorCategory::COUNT`].
    alerts: Mutex<[AlertWindow; ErrorCategory::COUNT]>,
}

/// Rolling-window bookkeeping for one error category (O6): how many errors
/// were seen, how many log lines were admitted, and whether the alert
/// threshold already fired in the current window.
#[derive(Debug, Default, Clone, Copy)]
struct AlertWindow {
    count: u32,
    logged: u32,
    window_started: Option<Instant>,
    alerted: bool,
}

/// The emit funnel's per-error admission decision (O6).
#[derive(Debug, Clone, Copy)]
pub enum Admit {
    /// Log the record normally.
    Log,
    /// The window's log budget is spent: skip the log line (the ring and
    /// the counters already captured the error).
    Suppress,
    /// Log the record, then emit one window alert with this error count.
    Alert(u32),
}

impl Metrics {
    pub fn new(latencies: bool) -> Self {
        Self {
            latencies,
            state_transitions: AtomicU64::new(0),
            notifies: AtomicU64::new(0),
            apply_retries: AtomicU64::new(0),
            logic_errors: AtomicU64::new(0),
            timeout_errors: AtomicU64::new(0),
            transport_errors: AtomicU64::new(0),
            resolution: LatencyStat::default_stat(),
            update: LatencyStat::default_stat(),
            cleanup: LatencyStat::default_stat(),
            drain: LatencyStat::default_stat(),
            recent: Mutex::new(VecDeque::new()),
            alerts: Mutex::new(std::array::from_fn(|_| AlertWindow::default())),
        }
    }

    /// The gate for latency instrumentation: `Some` starts a sample, `None`
    /// means instrumentation is off and the site skips timing entirely.
    pub fn instant_if(&self) -> Option<Instant> {
        self.latencies.then(Instant::now)
    }

    /// Records one latency sample for `slot`; a no-op when off (so callers
    /// may hold an `Instant` from [`Metrics::instant_if`] unconditionally).
    pub fn record(&self, slot: fn(&Self) -> &LatencyStat, started: Instant) {
        if self.latencies {
            slot(self).record(started.elapsed());
        }
    }

    /// Copies one graded error record into the ring (O5), dropping the
    /// oldest entry beyond the cap. Called from the S5 emit funnel only —
    /// every kernel error passes there exactly once.
    pub(crate) fn push_error(&self, class: ErrorClass, record: &LogRecord) {
        let entry = RecentError {
            class,
            category: record.category,
            fiber: record.fiber.clone(),
            uid: record.uid,
            trace: record.trace,
            event: record.event.to_string(),
            message: record.message.clone(),
            at: record.timestamp,
        };
        let mut ring = self.recent.lock();
        if ring.len() == RECENT_ERROR_CAP {
            ring.pop_front();
        }
        ring.push_back(entry);
    }

    /// Ring snapshot, oldest first (O5).
    pub(crate) fn recent_errors(&self) -> Vec<RecentError> {
        self.recent.lock().iter().cloned().collect()
    }

    /// Admits one error of `category` under the category's alert policy
    /// (O6): rolls the window when expired, counts the error, and decides
    /// whether the log line runs, is suppressed, or crosses the alert
    /// threshold. The alert fires exactly once per window, at the crossing.
    pub(crate) fn admit_alert(&self, category: ErrorCategory, alert: &CategoryAlert) -> Admit {
        let now = Instant::now();
        {
            let mut slots = self.alerts.lock();
            let slot = &mut slots[category.index()];
            let expired = slot
                .window_started
                .is_none_or(|started| now.duration_since(started) >= alert.window);
            if expired {
                *slot = AlertWindow::default();
                slot.window_started = Some(now);
            }
            slot.count = slot.count.saturating_add(1);
            let crossed = alert.alert_at.is_some_and(|at| slot.count == at);
            let budget_spent = alert.max_logged.is_some_and(|max| slot.logged >= max);
            let admit = if budget_spent {
                if crossed && !slot.alerted {
                    slot.alerted = true;
                    Admit::Alert(slot.count)
                } else {
                    Admit::Suppress
                }
            } else {
                slot.logged = slot.logged.saturating_add(1);
                if crossed && !slot.alerted {
                    slot.alerted = true;
                    Admit::Alert(slot.count)
                } else {
                    Admit::Log
                }
            };
            drop(slots);
            admit
        }
    }
}

/// Count, total, max, and the fixed bucket histogram of one latency metric.
///
/// Buckets are upper-bounded by `BUCKET_BOUNDS_NS`: samples under 1us,
/// 10us, 100us, 1ms, 10ms, 100ms, 1s, and the catch-all at one second or
/// more. `count == 0` means no samples (latency instrumentation off, or the
/// path never ran).
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencySummary {
    /// Recorded samples.
    pub count: u64,
    /// Sum of all samples (`total / count` is the mean when `count > 0`).
    pub total: Duration,
    /// Slowest observed sample.
    pub max: Duration,
    /// Per-bucket sample counts, bounds ascending.
    pub buckets: [u64; 8],
}

/// Point-in-time runtime statistics (O2).
///
/// A consistent-enough read for capacity and bottleneck answers: the fiber
/// distribution is one registry walk, counters are single atomics. Hosts
/// poll this (e.g. behind an HTTP endpoint) and wire exporters from it.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetricsSnapshot {
    /// Fibers in any state other than [`FiberState::Disposed`](crate::FiberState).
    pub live_fibers: u64,
    /// Fibers currently in `Pending`.
    pub pending_fibers: u64,
    /// Fibers currently in `Loading`.
    pub loading_fibers: u64,
    /// Fibers currently `Active`.
    pub active_fibers: u64,
    /// Fibers currently `Failed` (parked on a startup error).
    pub failed_fibers: u64,
    /// Fibers currently `Unloading`.
    pub unloading_fibers: u64,
    /// `(name, label)` slots in the reflect store.
    pub provider_count: u64,
    /// Pending bounded listener invocations (P2 queue capacity accounting).
    pub queue_depth: u64,
    /// Listener invocations dropped by full bounded lanes since start.
    pub dropped_events: u64,
    /// Lifecycle transitions since start (O1 log stream carries the pairs).
    pub state_transition_count: u64,
    /// `notify` calls since start (one call may fan out to many fibers).
    pub notify_count: u64,
    /// Dependency-edge wiring events since start (commits + uncommits).
    pub dependency_rebuild_count: u64,
    /// S3 apply-budget retries since start.
    pub apply_retry_count: u64,
    /// Error-class counts, stamped at every policy-graded error record.
    pub logic_error_count: u64,
    /// Timeout-class error records since start (S5 emit site).
    pub timeout_error_count: u64,
    /// Transport-class error records since start (S5 emit site).
    pub transport_error_count: u64,
    /// Latency summaries; `count == 0` when instrumentation is off.
    pub resolution: LatencySummary,
    /// `Fiber::update` wall time (config round trip + convergence).
    pub update: LatencySummary,
    /// Effect-disposer wall time (per disposer, including timed-out ones).
    pub cleanup: LatencySummary,
    /// Consumer-drain wall time per withdrawal drain call.
    pub drain: LatencySummary,
}
