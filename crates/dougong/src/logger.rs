// Rust guideline compliant 2026-10-20
//! Minimal logging facade installed as the built-in `logger` service.
//!
//! The kernel never logs directly: every log call resolves the `logger`
//! service, so plugins can replace or isolate loggers like any other service
//! (TS `LoggerService`, redoop `Logger` trait).

use std::io::Write;
use std::sync::Arc;

use crate::error::Error;

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

/// Pluggable log sink.
pub trait Logger: Send + Sync {
    /// Writes one structured record.
    fn log(&self, level: LogLevel, target: &str, message: &str);

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
