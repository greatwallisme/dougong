// Rust guideline compliant 2026-10-24
//! Error types for the dougong kernel.

use serde_json::Value;

/// The runtime policy class of an error (stability track S5, design D20).
///
/// Every kernel error belongs to exactly one class; hosts grade log levels
/// and failure reactions per class through
/// [`ErrorPolicies`](crate::ErrorPolicies). Ordered by severity so an
/// [`Error::aggregate`] can take the maximum of its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorClass {
    /// A failure inside the semantic model: capability and config errors,
    /// lifecycle guard rejections, business errors returned by plugins, and
    /// in-process panics (a plugin bug is a logic fault of that plugin).
    Logic,
    /// A wait missed its budget (S3), or a caller-side convergence wait
    /// expired.
    Timeout,
    /// The mechanics of crossing a plugin boundary failed: dylib FFI status
    /// and load failures, wasm traps, payload codec failures. A business
    /// error successfully transported back is *not* in this class.
    Transport,
}

impl ErrorClass {
    /// The stable lowercase name used in structured output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Logic => "logic",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
        }
    }
}

/// Kernel error with a stable machine-readable code.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}: {message}")]
pub struct Error {
    /// Stable machine-readable identifier, e.g. `"inactive_effect"`.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
    /// Runtime policy class (S5); set by the constructor.
    pub class: ErrorClass,
}

impl Error {
    /// Builds a `Logic`-class error from a code constant and a message.
    ///
    /// `Logic` is the right default for everything inside the semantic
    /// model; boundary-transport failures use [`Error::transport`], budget
    /// expiries use the dedicated timeout constructors.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            class: ErrorClass::Logic,
        }
    }

    /// Builds a `Transport`-class error: the mechanics of crossing a plugin
    /// boundary failed (FFI status, artifact load, trap, payload codec).
    #[must_use]
    pub fn transport(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            class: ErrorClass::Transport,
        }
    }

    /// Creates an `inactive_effect` error: an effect was
    /// requested on a fiber that can no longer accept effects.
    #[must_use]
    pub fn inactive_effect() -> Self {
        Self::new(
            "inactive_effect",
            "cannot create effect on inactive context",
        )
    }

    /// Creates a `budget_exceeded` error: `what` on fiber `name` crossed its
    /// configured cap of `cap` (plugin budgets, P7).
    #[must_use]
    pub fn budget_exceeded(what: &str, name: &str, cap: u64) -> Self {
        Self::new(
            "budget_exceeded",
            format!("{what} budget of {cap} exceeded on fiber \"{name}\""),
        )
    }

    /// Creates an "inactive dependency" error for `name`.
    #[must_use]
    pub fn inactive_dependency(name: &str) -> Self {
        Self::new(
            "inactive_dependency",
            format!("cannot get required service \"{name}\" in inactive context"),
        )
    }

    /// Creates an "undeclared dependency" error for `name`.
    #[must_use]
    pub fn undeclared_dependency(name: &str) -> Self {
        Self::new(
            "undeclared_dependency",
            format!("cannot get property \"{name}\" without inject"),
        )
    }

    /// Creates a duplicate-provider error for `name`.
    #[must_use]
    pub fn duplicate_provide(name: &str, owner: &str) -> Self {
        Self::new(
            "duplicate_provide",
            format!("service \"{name}\" has been registered at <{owner}>"),
        )
    }

    /// Creates a "not the provider" error for `name`.
    #[must_use]
    pub fn not_provider(name: &str) -> Self {
        Self::new(
            "not_provider",
            format!("cannot set property \"{name}\" in multiple fibers"),
        )
    }

    /// Wraps an aggregation of listener failures (parallel dispatch). The
    /// class is the most severe among the members.
    #[must_use]
    pub fn aggregate(errors: &[Self]) -> Self {
        let message = errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        let class = errors
            .iter()
            .map(|err| err.class)
            .max()
            .unwrap_or(ErrorClass::Logic);
        Self {
            code: "aggregate",
            message,
            class,
        }
    }

    /// Creates an `apply_timeout` error: the plugin body of `name` missed its
    /// activation budget (stability track S3).
    #[must_use]
    pub fn apply_timeout(name: &str) -> Self {
        Self {
            code: "apply_timeout",
            message: format!("plugin \"{name}\" body timed out during activation"),
            class: ErrorClass::Timeout,
        }
    }

    /// Creates a `listener_timeout` error: a listener on `name` missed its
    /// invocation budget (stability track S3).
    #[must_use]
    pub fn listener_timeout(name: &str) -> Self {
        Self {
            code: "listener_timeout",
            message: format!("listener for \"{name}\" timed out"),
            class: ErrorClass::Timeout,
        }
    }

    /// Creates a `join_timeout` error: a caller-side convergence wait on
    /// `name` expired (`FiberHandle::join_with_timeout`).
    #[must_use]
    pub fn join_timeout(name: &str) -> Self {
        Self {
            code: "join_timeout",
            message: format!("join(\"{name}\") timed out"),
            class: ErrorClass::Timeout,
        }
    }

    /// Creates a `plugin_panicked` error: an in-process plugin body panicked
    /// (a logic fault of that plugin, contained by the task boundary).
    #[must_use]
    pub fn plugin_panicked(text: &str) -> Self {
        Self::new("plugin_panicked", format!("plugin panicked: {text}"))
    }

    /// Creates a `plugin_cancelled` error: the plugin body task was cancelled
    /// before it settled (a dispose raced the load).
    #[must_use]
    pub fn plugin_cancelled() -> Self {
        Self::new("plugin_cancelled", "plugin task was cancelled")
    }

    /// Creates a `listener_panicked` error: a listener invocation task died
    /// with a join failure.
    #[must_use]
    pub fn listener_panicked(detail: impl std::fmt::Display) -> Self {
        Self::new(
            "listener_panicked",
            format!("listener task failed: {detail}"),
        )
    }

    /// Creates a `fiber_disposed` error: `action` was requested on a fiber
    /// whose disposal already completed (lifecycle guard, stability track S4).
    #[must_use]
    pub fn fiber_disposed(action: &str) -> Self {
        Self::new(
            "fiber_disposed",
            format!("cannot {action} a disposed fiber"),
        )
    }

    /// Creates a `fiber_unloading` error: `action` was requested while the
    /// fiber is draining effects (lifecycle guard, stability track S4).
    #[must_use]
    pub fn fiber_unloading(action: &str) -> Self {
        Self::new(
            "fiber_unloading",
            format!("cannot {action} a fiber that is unloading"),
        )
    }

    /// Creates a `fiber_loading` error: `action` was requested while an
    /// activation is in flight (lifecycle guard, stability track S4).
    #[must_use]
    pub fn fiber_loading(action: &str) -> Self {
        Self::new(
            "fiber_loading",
            format!("cannot {action} a fiber that is loading"),
        )
    }

    /// Builds a generic message-only error (`internal` code, `Logic` class).
    #[must_use]
    pub fn msg(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }
}

/// Convenient result alias for kernel operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Converts a configuration value into a typed payload, reporting the JSON
/// path of failures.
///
/// This is the single validation seam of the kernel: v1 accepts any `Value`
/// and only enforces JSON-ness; structured schema validation is layered above
/// by callers.
pub fn require_object(config: &Value) -> Result<()> {
    if config.is_null() || config.is_object() {
        Ok(())
    } else {
        Err(Error::new(
            "invalid_config",
            "config must be an object or null",
        ))
    }
}
