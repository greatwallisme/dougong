// Rust guideline compliant 2026-10-24
//! Timeout budgets for the kernel's critical async waits (stability track S3).
//!
//! Every budget is optional; `None` (the default) preserves the unbounded
//! pre-S3 wait, so an existing host observes zero behavior change until it
//! opts in through [`Context::new_with_timeouts`]. The mechanism lives in the
//! kernel; the policy values are host wiring.
//!
//! What a timeout *does* depends on the future class of the timed-out work:
//!
//! - **abort-safe** work (in-process plugin bodies, plain listeners, plain
//!   disposers) is dropped at its next yield point — the same granularity as
//!   the S2 cancel scope.
//! - **tracked** work (boundary bridge calls, wasm bodies) is never dropped
//!   mid-flight: the kernel stops *waiting*, records the timeout, and the
//!   foreign call keeps running inside the listener fiber's cancel scope, so
//!   the unload reap still accounts for it. A synchronous foreign call that
//!   never returns cannot be preempted by any task mechanism; the
//!   [`TimeoutConfig::reap_watchdog`] surfaces that class.
//!
//! [`Context::new_with_timeouts`]: crate::Context::new_with_timeouts

use std::time::Duration;

/// Timeout budgets for the kernel's critical async paths (S3).
///
/// All fields default to `None` (unbounded, the pre-S3 behavior).
#[derive(Debug, Clone, Default)]
pub struct TimeoutConfig {
    /// Bounds one plugin-body activation attempt on the load path. The
    /// `internal/config` waterfall that precedes the body is bounded by
    /// [`TimeoutConfig::listener`] instead.
    pub apply: Option<ApplyTimeout>,
    /// Bounds one listener invocation, per listener, in every dispatch mode.
    ///
    /// In `parallel`, a timed-out listener counts as one failure in the
    /// aggregate; in `serial`/`bail`/`waterfall` the timeout propagates as a
    /// `listener_timeout` error; in `emit` it is logged (fire-and-forget).
    /// The budget is per invocation, so a dispatch over N listeners can wait
    /// up to N budgets in the worst case.
    pub listener: Option<Duration>,
    /// Bounds the per-consumer convergence wait inside a provider withdrawal
    /// (the drain protocol). A timeout weakens the withdrawal guarantee to a
    /// bounded wait; the policy decides whether the stuck consumer's task
    /// scope is also cancelled.
    pub drain: Option<DrainTimeout>,
    /// Bounds one *orphanable* effect disposer during unload or eager
    /// disposal. A timed-out disposer keeps running on a detached task (it is
    /// never aborted: aborting a wasm teardown would drop an in-flight call,
    /// and aborting a synchronous foreign call is impossible), the timeout is
    /// logged, and the LIFO drain continues.
    ///
    /// Kernel-structural disposers (service withdrawal, listener removal, the
    /// parent cascade) are never orphaned: their inner waits are bounded by
    /// their own budgets ([`TimeoutConfig::drain`]) or are synchronous.
    ///
    /// Contract: under a cleanup budget a plugin's teardown may overlap its
    /// next activation; effects that require strict teardown-before-reload
    /// must not rely on this budget.
    pub cleanup: Option<Duration>,
    /// Watchdog for the cancel-scope reap inside unload and activation
    /// rollback. On each elapsed interval an error is logged with the number
    /// of tracked tasks still in flight, then the reap keeps waiting —
    /// proceeding to teardown while a foreign call is in flight is documented
    /// UB for dylib instances, so this budget only ever *reports*.
    pub reap_watchdog: Option<Duration>,
}

/// Body-activation budget plus the action taken when it fires.
#[derive(Debug, Clone, Copy)]
pub struct ApplyTimeout {
    /// The per-attempt budget.
    pub budget: Duration,
    /// What happens after a timed-out attempt has been rolled back.
    pub policy: ApplyPolicy,
}

impl ApplyTimeout {
    /// Fail the activation once the budget elapses (the fiber parks `Failed`
    /// with an `apply_timeout` error; the runtime stays alive).
    #[must_use]
    pub const fn fail_after(budget: Duration) -> Self {
        Self {
            budget,
            policy: ApplyPolicy::Fail,
        }
    }

    /// Re-attempt the activation up to `retries` times, then park `Failed`.
    ///
    /// Retrying is sound because activation is transactional: the timed-out
    /// attempt's effects are rolled back LIFO before the retry runs.
    #[must_use]
    pub const fn retry_after(budget: Duration, retries: u32) -> Self {
        Self {
            budget,
            policy: ApplyPolicy::Retry(retries),
        }
    }
}

/// What a body-activation timeout does after the attempt is rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyPolicy {
    /// Park the fiber `Failed` with an `apply_timeout` error.
    Fail,
    /// Re-run the activation up to this many times, then park `Failed`.
    Retry(u32),
}

/// Drain-wait budget plus the action taken when one consumer times out.
#[derive(Debug, Clone, Copy)]
pub struct DrainTimeout {
    /// The per-consumer budget.
    pub budget: Duration,
    /// What happens when one consumer fails to converge in time.
    pub policy: DrainPolicy,
}

impl DrainTimeout {
    /// Log the timed-out consumer and keep draining the rest.
    #[must_use]
    pub const fn log_after(budget: Duration) -> Self {
        Self {
            budget,
            policy: DrainPolicy::LogAndContinue,
        }
    }

    /// Additionally cancel the timed-out consumer's task scope (the "kill
    /// task" policy): an abort-safe hang lands the consumer `Failed`; a
    /// tracked foreign call is unaffected but unreported here — the reap
    /// watchdog covers it.
    #[must_use]
    pub const fn cancel_after(budget: Duration) -> Self {
        Self {
            budget,
            policy: DrainPolicy::CancelConsumer,
        }
    }
}

/// What a drain timeout does with the consumer that missed its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainPolicy {
    /// Log and continue draining the remaining consumers.
    LogAndContinue,
    /// Cancel the consumer's task scope, then log and continue.
    CancelConsumer,
}
