// Rust guideline compliant 2026-10-21
//! Mapping between the kernel `Error` and the WIT `kernel-error` variant,
//! plus the payload contract hash shared by host and guest.

use crate::bindings::dougong::kernel::types as wit;

/// Contract hash of the built-in dynamic channel (interface + encoding).
///
/// Both sides of the boundary stamp service/event keys with it; the kernel
/// keys services by plain name, the hash rides along for cross-version
/// diagnostics.
pub const CONTRACT_ABI_HASH: &[u8] = b"dougong-dynamic-channel-v1";

/// Kernel error to WIT: keeps the two-level resolution distinction of TS
/// Algorithm 6 and degrades everything else to `internal`.
pub fn to_wit(err: &dougong::Error, key: Option<&wit::ServiceKey>) -> wit::KernelError {
    match err.code {
        "inactive_effect" => wit::KernelError::InactiveContext,
        "inactive_dependency" => key.map_or(wit::KernelError::InactiveContext, |key| {
            wit::KernelError::InactiveDependency(key.clone())
        }),
        "undeclared_dependency" => key.map_or_else(
            || wit::KernelError::CapabilityDenied(err.message.clone()),
            |key| wit::KernelError::UndeclaredDependency(key.clone()),
        ),
        _ => wit::KernelError::Internal(format!("{}: {}", err.code, err.message)),
    }
}

/// WIT kernel-error to kernel error: inverse of [`to_wit`].
pub fn to_kernel(err: &wit::KernelError) -> dougong::Error {
    match err {
        wit::KernelError::InactiveContext => dougong::Error::inactive_effect(),
        wit::KernelError::InactiveDependency(key) => dougong::Error::inactive_dependency(&key.name),
        wit::KernelError::UndeclaredDependency(key) => {
            dougong::Error::undeclared_dependency(&key.name)
        }
        wit::KernelError::CapabilityDenied(message) => {
            dougong::Error::new("capability_denied", message.clone())
        }
        wit::KernelError::Internal(message) => dougong::Error::msg(message.clone()),
    }
}

/// WIT log level to kernel log level.
pub const fn to_log_level(level: wit::LogLevel) -> dougong::LogLevel {
    match level {
        wit::LogLevel::Debug => dougong::LogLevel::Debug,
        wit::LogLevel::Info => dougong::LogLevel::Info,
        wit::LogLevel::Warn => dougong::LogLevel::Warn,
        wit::LogLevel::Error => dougong::LogLevel::Error,
    }
}

/// Encodes a kernel JSON payload onto the wire.
pub fn encode(value: &serde_json::Value) -> dougong::Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|err| dougong::Error::transport("payload_codec", format!("encode payload: {err}")))
}

/// Decodes a wire payload into a kernel JSON value.
pub fn decode(bytes: &[u8]) -> dougong::Result<serde_json::Value> {
    serde_json::from_slice(bytes)
        .map_err(|err| dougong::Error::transport("payload_codec", format!("decode payload: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> wit::ServiceKey {
        wit::ServiceKey {
            name: name.to_string(),
            abi_hash: CONTRACT_ABI_HASH.to_vec(),
        }
    }

    #[test]
    fn wit_mapping_preserves_the_two_level_distinction() {
        let err = dougong::Error::undeclared_dependency("svc");
        assert!(matches!(
            to_wit(&err, Some(&key("svc"))),
            wit::KernelError::UndeclaredDependency(_)
        ));
        let err = dougong::Error::inactive_dependency("svc");
        assert!(matches!(
            to_wit(&err, Some(&key("svc"))),
            wit::KernelError::InactiveDependency(_)
        ));
        let roundtrip = to_kernel(&to_wit(
            &dougong::Error::undeclared_dependency("svc"),
            Some(&key("svc")),
        ));
        assert_eq!(roundtrip.code, "undeclared_dependency");
    }

    #[test]
    fn transport_codes_survive_the_internal_variant() {
        // The WIT variant set is contract-frozen (D7): a boundary failure
        // degrades to `internal`, but its stable code keeps the error's type
        // explicit and recoverable on the wire (S5/D20).
        let err = dougong::Error::transport("wasm_trap", "guest trapped");
        let wit::KernelError::Internal(message) = to_wit(&err, None) else {
            panic!("transport errors map to internal");
        };
        assert_eq!(message, "wasm_trap: guest trapped");
        let back = to_kernel(&wit::KernelError::Internal(message));
        assert_eq!(back.code, "internal");
        assert!(back.message.contains("wasm_trap"));
    }
}
