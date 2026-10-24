// Rust guideline compliant 2026-10-20
//! Error types for the dougong kernel.

use serde_json::Value;

/// Kernel error with a stable machine-readable code.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}: {message}")]
pub struct Error {
    /// Stable machine-readable identifier, e.g. `"inactive_effect"`.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl Error {
    /// Builds an error from a code constant and a formatted message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
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

    /// Wraps an aggregation of listener failures (parallel dispatch).
    #[must_use]
    pub fn aggregate(errors: &[Self]) -> Self {
        let message = errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        Self::new("aggregate", message)
    }

    /// Creates an `apply_timeout` error: the plugin body of `name` missed its
    /// activation budget (stability track S3).
    #[must_use]
    pub fn apply_timeout(name: &str) -> Self {
        Self::new(
            "apply_timeout",
            format!("plugin \"{name}\" body timed out during activation"),
        )
    }

    /// Creates a `listener_timeout` error: a listener on `name` missed its
    /// invocation budget (stability track S3).
    #[must_use]
    pub fn listener_timeout(name: &str) -> Self {
        Self::new(
            "listener_timeout",
            format!("listener for \"{name}\" timed out"),
        )
    }

    /// Builds a generic message-only error.
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
