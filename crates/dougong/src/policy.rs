// Rust guideline compliant 2026-10-24
//! Per-class error policy surface (stability track S5, design D20).
//!
//! Every kernel [`Error`] carries an [`ErrorClass`]; this module holds the
//! per-class reaction table the host wires in at construction. All defaults
//! reproduce the pre-S5 behavior exactly (everything logs at
//! [`LogLevel::Error`], listener failures are log-only, no provider
//! quarantine), so an untouched configuration is a zero-behavior-change
//! guarantee.
//!
//! The four graded knobs map to honest seams only:
//!
//! - **log level**: every kernel error log line routes through
//!   [`ErrorPolicies::log`] / [`ErrorPolicies::log_error`].
//! - **crash**: a listener failure can additionally restart or dispose the
//!   *offending* fiber (never the emitter); the plugin body's transactional
//!   `Failed` parking is fixed semantics, not a knob.
//! - **retry**: bounded apply retries are the timeout class's knob and live
//!   in [`crate::TimeoutConfig`] (S3). Listener and transport calls are
//!   never auto-retried: delivery counts are dispatch-mode contract and the
//!   kernel has no idempotency signal, so retries belong to the emitter's
//!   business logic.
//! - **isolate**: [`ProviderQuarantine::DisposeOnTransportError`] disposes a
//!   provider fiber whose cross-boundary calls fail with a
//!   [`ErrorClass::Transport`] error, so a broken boundary instance is torn
//!   down instead of erroring forever (a loader's next reconcile may
//!   deliberately re-add it).

use std::sync::{Arc, Weak};

use crate::context::{Context, RootState};
use crate::error::{Error, ErrorClass, Result};
use crate::fiber::Fiber;
use crate::invoke::{Invoke, Invoker};
use crate::logger::{LogLevel, LogRecord, Logger};
use crate::plugin::BoxFuture;

/// What the runtime does with the fiber whose listener invocation failed.
///
/// The reaction is additional to — never a replacement for — the dispatch
/// mode's own propagation semantics: fire-and-forget stays log-only towards
/// the emitter, and the chaining modes keep propagating the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListenerFailurePolicy {
    /// Log the failure; the offending fiber is left alone (pre-S5 behavior).
    #[default]
    LogOnly,
    /// Restart the offending fiber (a fresh activation attempt). Event-rate
    /// bound: every later dispatch to a structurally failing listener
    /// restarts it again, so pair this with budgets that keep events sparse.
    RestartFiber,
    /// Dispose the offending fiber (terminal, S4-idempotent). The harshest
    /// isolation: the plugin leaves the runtime until a host re-adds it.
    DisposeFiber,
}

/// Isolation reaction to a transport-class failure of a provided service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProviderQuarantine {
    /// The error simply returns to the caller (pre-S5 behavior).
    #[default]
    Disabled,
    /// Dispose the provider fiber when one of its dynamic-channel calls
    /// fails with a [`ErrorClass::Transport`] error. In-process
    /// implementations never produce that class, so the knob only ever fires
    /// on boundary-provided services.
    DisposeOnTransportError,
}

/// The reaction table of one error class.
#[derive(Debug, Clone, Copy)]
pub struct ClassPolicy {
    /// Level at which failures of this class are logged.
    pub log_level: LogLevel,
    /// Reaction applied to the offending fiber when one of its listener
    /// invocations settles with a failure of this class.
    pub on_listener_failure: ListenerFailurePolicy,
}

impl Default for ClassPolicy {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Error,
            on_listener_failure: ListenerFailurePolicy::LogOnly,
        }
    }
}

/// Per-class error policies, fixed at root construction (S5).
///
/// Wire them in through
/// [`Context::new_with_options`](crate::Context::new_with_options); the
/// default table reproduces the pre-S5 behavior.
#[derive(Debug, Clone, Default)]
pub struct ErrorPolicies {
    /// Reactions for [`ErrorClass::Logic`] failures.
    pub logic: ClassPolicy,
    /// Reactions for [`ErrorClass::Timeout`] failures.
    pub timeout: ClassPolicy,
    /// Reactions for [`ErrorClass::Transport`] failures.
    pub transport: ClassPolicy,
    /// Isolation reaction for transport-class failures of provided services.
    pub provider_quarantine: ProviderQuarantine,
}

impl ErrorPolicies {
    /// The reaction table of `class`.
    #[must_use]
    pub const fn for_class(&self, class: ErrorClass) -> &ClassPolicy {
        match class {
            ErrorClass::Logic => &self.logic,
            ErrorClass::Timeout => &self.timeout,
            ErrorClass::Transport => &self.transport,
        }
    }

    /// Stamps the class-graded level and the class onto `record`, then
    /// writes it through the structured face (O1). Every kernel error log
    /// line funnels here so grading stays single-sourced.
    pub(crate) fn emit(&self, logger: &dyn Logger, class: ErrorClass, mut record: LogRecord) {
        record.level = self.for_class(class).log_level;
        record.class = Some(class);
        logger.log_record(&record);
    }

    /// Logs `err` at the level the policy assigns to its class.
    pub(crate) fn log_error(&self, logger: &dyn Logger, err: &Error) {
        self.emit(
            logger,
            err.class,
            LogRecord::new(
                self.for_class(err.class).log_level,
                "dougong",
                "error",
                err.to_string(),
            ),
        );
    }
}

/// Applies the listener-failure reaction of `err`'s class to the offending
/// fiber (S5 crash knob).
///
/// The reaction runs on a detached task: dispatch latency must not pay for a
/// fiber convergence, and an inline `dispose` could join fibers whose
/// in-flight work leads back to this dispatch. A listener registered on the
/// root context has no fiber and is never acted on.
pub fn react_to_listener_failure(ctx: &Context, fiber: Option<&Weak<Fiber>>, err: &Error) {
    let action = ctx.root.policies.for_class(err.class).on_listener_failure;
    if action == ListenerFailurePolicy::LogOnly {
        return;
    }
    let Some(fiber) = fiber.and_then(Weak::upgrade) else {
        return;
    };
    // The root fiber (uid 0) is immortal and owns the built-in listeners;
    // acting on it would tear down the framework itself.
    if fiber.uid_value() == Some(0) {
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let outcome = match action {
            ListenerFailurePolicy::LogOnly => Ok(()),
            ListenerFailurePolicy::RestartFiber => fiber.restart().await,
            ListenerFailurePolicy::DisposeFiber => {
                fiber.dispose().await;
                Ok(())
            }
        };
        if let Err(action_err) = outcome {
            ctx.root.policies.log_error(&*ctx.logger(), &action_err);
        }
    });
}

/// `Invoke` decorator that quarantines the provider on a transport-class
/// failure (S5 isolate knob). Wraps exactly once per resolution, only when
/// [`ProviderQuarantine::DisposeOnTransportError`] is configured.
struct QuarantineInvoke {
    inner: Arc<dyn Invoke>,
    provider_uid: u64,
    service: Arc<str>,
    root: Weak<RootState>,
}

impl Invoke for QuarantineInvoke {
    fn call(&self, method: u32, payload: Vec<u8>) -> BoxFuture<Result<Vec<u8>>> {
        let inner = Arc::clone(&self.inner);
        let root = Weak::clone(&self.root);
        let provider_uid = self.provider_uid;
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            let result = inner.call(method, payload).await;
            let Err(err) = &result else { return result };
            if err.class != ErrorClass::Transport {
                return result;
            }
            let Some(root) = root.upgrade() else {
                return result;
            };
            let fiber = root.fiber_by_uid(provider_uid);
            let Some(fiber) = fiber else { return result };
            // Log through the provider fiber's own context so the record
            // lands in the provider realm's logger (isolation-consistent
            // with every other kernel log line); `fiber` is a local Arc, so
            // the weak-root wrapper never keeps the provider alive itself.
            root.policies.emit(
                &*fiber.ctx().logger(),
                ErrorClass::Transport,
                LogRecord::new(
                    LogLevel::Error,
                    "policy",
                    "provider_quarantine",
                    "disposing provider fiber after transport-class call failure".to_string(),
                )
                .with_fiber(fiber.name(), Some(provider_uid))
                .with_service(&*service)
                .with_cause(err.to_string()),
            );
            // Detached (C7-analog): the caller's own fiber may be drained by
            // this disposal, so the disposal must not be awaited here.
            drop(tokio::spawn(async move { fiber.dispose().await }));
            result
        })
    }
}

/// Wraps `value` in the quarantine decorator when it is an [`Invoker`] and
/// the isolate knob is on; returns the input untouched otherwise. `service`
/// is the name the value is stored under, carried into quarantine records.
pub fn maybe_quarantine(
    root: &Arc<RootState>,
    provider_uid: u64,
    service: &str,
    value: Arc<dyn std::any::Any + Send + Sync>,
) -> Arc<dyn std::any::Any + Send + Sync> {
    if root.policies.provider_quarantine == ProviderQuarantine::Disabled {
        return value;
    }
    let Ok(invoker) = Arc::clone(&value).downcast::<Invoker>() else {
        return value;
    };
    Arc::new(Invoker::new(Arc::new(QuarantineInvoke {
        inner: Arc::clone(&invoker.inner),
        provider_uid,
        service: Arc::from(service),
        root: Arc::downgrade(root),
    })))
}
