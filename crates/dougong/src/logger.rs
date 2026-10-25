// Rust guideline compliant 2026-10-25
//! Minimal logging facade installed as the built-in `logger` service.
//!
//! The kernel never logs directly: every log call resolves the `logger`
//! service, so plugins can replace or isolate loggers like any other service
//! (TS `LoggerService`, redoop `Logger` trait).
//!
//! Since O1 (improve 三.1) the kernel logs through [`LogRecord`] instead of
//! flat strings: each record carries the mandatory face (level, target,
//! event kind, message, timestamp) plus whichever optional context fields
//! the site knows, so a structured sink can answer "which fiber, which
//! service, from which state to which state, since when" without parsing
//! prose. Sinks that only implement [`Logger::log`] still receive every
//! record through the flat degradation in [`Logger::log_record`], which is
//! why the plain-text format of the pre-O1 lines is preserved.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::error::{Error, ErrorCategory, ErrorClass};
use crate::events::DispatchMode;
use crate::fiber::FiberState;

/// Log severity levels, ordered from most to least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Error conditions.
    Error,
    /// Warning conditions.
    Warn,
    /// Informational messages.
    Info,
    /// Debug output.
    Debug,
    /// Trace output.
    Trace,
}

impl LogLevel {
    /// The stable lowercase name used in log output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// One structured log record (O1, improve 三.1).
///
/// Mandatory face: [`LogRecord::level`], [`LogRecord::target`],
/// [`LogRecord::event`], [`LogRecord::message`],
/// [`LogRecord::timestamp`]. Everything else is optional context a site
/// fills when it knows the value; `None` never means "zero", only "not
/// applicable here".
#[derive(Debug, Clone)]
pub struct LogRecord {
    /// Severity; error-class records get it graded by `ErrorPolicies`.
    pub level: LogLevel,
    /// Kernel subsystem that produced the record (`"fiber"`, `"events"`,
    /// `"service"`, `"policy"`).
    pub target: &'static str,
    /// Stable record kind (`"state_transition"`, `"cleanup_timeout"`, ...).
    pub event: &'static str,
    /// Human-readable summary, verbatim from the pre-O1 prose.
    pub message: String,
    /// Capture time, taken once at construction.
    pub timestamp: SystemTime,
    /// Error class, stamped by `ErrorPolicies::emit`.
    pub class: Option<ErrorClass>,
    /// Alerting group (O6), stamped by `ErrorPolicies::emit` next to the
    /// class — the fine-grained identity stays in the message's error code.
    pub category: Option<ErrorCategory>,
    /// Fiber / plugin name.
    pub fiber: Option<String>,
    /// Fiber uid; `None` also covers dispose-complete sites (uid cleared).
    pub uid: Option<u64>,
    /// Trace id (O3): the fiber's uid as captured at construction, stable
    /// across dispose — the join key that stitches a plugin's whole
    /// lifecycle (start to fail to teardown) into one chain. `None` only on
    /// records with no fiber context at all (the global error face).
    pub trace: Option<u64>,
    /// Boundary tag (O3): which plugin form owns the fiber (`"dylib"`,
    /// `"wasm"`); `None` for in-process plugins.
    pub boundary: Option<&'static str>,
    /// Isolation realm label; `None` = the default label.
    pub realm: Option<u64>,
    /// Service name.
    pub service: Option<String>,
    /// Event name.
    pub event_name: Option<String>,
    /// Dispatch mode, for records on the event path.
    pub mode: Option<DispatchMode>,
    /// Lifecycle state before a transition.
    pub from_state: Option<FiberState>,
    /// Lifecycle state after a transition.
    pub to_state: Option<FiberState>,
    /// Underlying error or join-error text.
    pub cause: Option<String>,
    /// Elapsed time or the budget that fired, whichever the site knows.
    pub duration: Option<Duration>,
}

impl LogRecord {
    /// Builds a record with the mandatory face and no context fields.
    #[must_use]
    pub fn new(
        level: LogLevel,
        target: &'static str,
        event: &'static str,
        message: String,
    ) -> Self {
        Self {
            level,
            target,
            event,
            message,
            timestamp: SystemTime::now(),
            class: None,
            category: None,
            fiber: None,
            uid: None,
            trace: None,
            boundary: None,
            realm: None,
            service: None,
            event_name: None,
            mode: None,
            from_state: None,
            to_state: None,
            cause: None,
            duration: None,
        }
    }

    /// Attaches the error class.
    #[must_use]
    pub const fn with_class(mut self, class: ErrorClass) -> Self {
        self.class = Some(class);
        self
    }

    /// Attaches the alerting group (O6).
    #[must_use]
    pub const fn with_category(mut self, category: Option<ErrorCategory>) -> Self {
        self.category = category;
        self
    }

    /// Attaches the fiber name and uid.
    #[must_use]
    pub fn with_fiber(mut self, fiber: String, uid: Option<u64>) -> Self {
        self.fiber = Some(fiber);
        self.uid = uid;
        self
    }

    /// Attaches the fiber identity when the record has one at all
    /// (root-registered listeners have no fiber); both fields clear together.
    #[must_use]
    pub fn with_fiber_opt(mut self, fiber: Option<String>, uid: Option<u64>) -> Self {
        self.fiber = fiber;
        self.uid = uid;
        self
    }

    /// Attaches the isolation realm label (`None` = default label).
    #[must_use]
    pub const fn with_realm(mut self, realm: Option<u64>) -> Self {
        self.realm = realm;
        self
    }

    /// Attaches the fiber's trace id.
    #[must_use]
    pub const fn with_trace(mut self, trace: Option<u64>) -> Self {
        self.trace = trace;
        self
    }

    /// Attaches the boundary tag of the fiber's plugin form.
    #[must_use]
    pub const fn with_boundary(mut self, boundary: Option<&'static str>) -> Self {
        self.boundary = boundary;
        self
    }

    /// Attaches the service name.
    #[must_use]
    pub fn with_service(mut self, service: impl Into<String>) -> Self {
        self.service = Some(service.into());
        self
    }

    /// Attaches the event name.
    #[must_use]
    pub fn with_event_name(mut self, event_name: impl Into<String>) -> Self {
        self.event_name = Some(event_name.into());
        self
    }

    /// Attaches the dispatch mode.
    #[must_use]
    pub const fn with_mode(mut self, mode: DispatchMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Attaches the from/to lifecycle states.
    #[must_use]
    pub const fn with_states(mut self, from: FiberState, to: FiberState) -> Self {
        self.from_state = Some(from);
        self.to_state = Some(to);
        self
    }

    /// Attaches the underlying error text.
    #[must_use]
    pub fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// Attaches a duration.
    #[must_use]
    pub const fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Renders the record the way the flat degradation sinks see it:
    /// `{event}: {message}` plus a `k=v` block for every present field, in
    /// a fixed order. The plain-text needle surface of the pre-O1 lines is
    /// preserved by construction.
    #[must_use]
    pub fn to_flat(&self) -> String {
        let mut line = format!("{}: {}", self.event, self.message);
        let mut fields = String::new();
        let mut push = |key: &str, value: String| {
            if !fields.is_empty() {
                fields.push(' ');
            }
            fields.push_str(key);
            fields.push('=');
            fields.push_str(&value);
        };
        if let Some(fiber) = &self.fiber {
            push("fiber", format!("{fiber:?}"));
        }
        if let Some(uid) = self.uid {
            push("uid", uid.to_string());
        }
        if let Some(trace) = self.trace {
            push("trace", trace.to_string());
        }
        if let Some(boundary) = self.boundary {
            push("boundary", boundary.to_string());
        }
        if let Some(realm) = self.realm {
            push("realm", realm.to_string());
        }
        if let Some(service) = &self.service {
            push("service", format!("{service:?}"));
        }
        if let Some(event_name) = &self.event_name {
            push("event", format!("{event_name:?}"));
        }
        if let Some(mode) = self.mode {
            push("mode", mode.as_str().to_string());
        }
        if let (Some(from), Some(to)) = (self.from_state, self.to_state) {
            push("state", format!("{}->{}", from.as_str(), to.as_str()));
        }
        if let Some(class) = self.class {
            push("class", class.as_str().to_string());
        }
        if let Some(category) = self.category {
            push("category", category.as_str().to_string());
        }
        if let Some(cause) = &self.cause {
            push("cause", format!("{cause:?}"));
        }
        if let Some(duration) = self.duration {
            push("duration", format!("{duration:?}"));
        }
        if !fields.is_empty() {
            line.push_str(" (");
            line.push_str(&fields);
            line.push(')');
        }
        line
    }
}

/// Pluggable log sink.
pub trait Logger: Send + Sync {
    /// Writes one flat record (the pre-O1 face, still the only mandatory
    /// method).
    fn log(&self, level: LogLevel, target: &str, message: &str);

    /// Writes one structured record.
    ///
    /// The default degrades to [`Logger::log`] through
    /// [`LogRecord::to_flat`], so sinks written before O1 keep working
    /// unchanged; structured sinks override this one method.
    fn log_record(&self, record: &LogRecord) {
        self.log(record.level, record.target, &record.to_flat());
    }

    /// Convenience wrapper for error records.
    fn log_error(&self, error: &Error) {
        self.log(LogLevel::Error, "dougong", &error.to_string());
    }
}

/// Standard-error logger used when no plugin replaced the `logger` service.
#[derive(Debug, Default)]
pub struct StderrLogger;

impl Logger for StderrLogger {
    fn log(&self, level: LogLevel, target: &str, message: &str) {
        let mut stderr = std::io::stderr().lock();
        // Best-effort sink: stderr write failures (e.g. closed pipe) are
        // ignored because logging must never take the process down.
        let _ = writeln!(stderr, "[{}] {target}: {message}", level.as_str());
    }
}

/// Wrapper stored in the service store so hosts can downcast it back.
pub struct LoggerHandle(pub Arc<dyn Logger>);

impl std::fmt::Debug for LoggerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoggerHandle").finish_non_exhaustive()
    }
}

impl LoggerHandle {
    /// Builds a handle around any logger implementation.
    #[must_use]
    pub fn new(logger: Arc<dyn Logger>) -> Self {
        Self(logger)
    }
}
