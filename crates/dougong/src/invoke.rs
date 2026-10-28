// Rust guideline compliant 2026-10-28
//! The dynamic service invocation channel (design decision D6).
//!
//! Services provided across a plugin boundary (dylib/so, wasm) are invoked
//! through a generic channel — `method` selector plus byte payload — instead
//! of sharing Rust types across compilation artifacts. In-process consumers
//! downcast the provided value to [`Invoker`] and call through it; the
//! boundary adapter (dylib export table, wasm component) implements the
//! actual transport.
//!
//! Interface identity rides on the service name plus the contract hash of the
//! descriptor that declared it (`service-key{interface, abi-hash}` at the
//! boundary; the kernel stores the plain name).

use std::sync::Arc;

use crate::error::Result;
use crate::plugin::BoxFuture;

/// A service implementation invocable through the dynamic channel.
///
/// Implemented by boundary adapters (dylib export tables, wasm component
/// hosts); in-process Rust plugins may also implement it to expose
/// interface-style services to non-Rust consumers.
pub trait Invoke: Send + Sync {
    /// Returns the stable activation identity used by boundary capability grants.
    ///
    /// Boundary hosts return a process-unique, never-reused id when one
    /// invocation adapter represents one plugin activation. Decorators must
    /// forward the wrapped implementation's id. The default `None` preserves
    /// ordinary in-process implementations, which do not participate in the
    /// dylib runtime-grant mechanism.
    fn activation_id(&self) -> Option<u64> {
        None
    }

    /// Invokes one method of the service.
    ///
    /// `payload` encoding is fixed by the service contract (the interface
    /// descriptor hash covers interface + encoding). Implementations must be
    /// non-blocking enough for the host's driver: long work belongs on the
    /// plugin side of the boundary.
    fn call(&self, method: u32, payload: Vec<u8>) -> BoxFuture<Result<Vec<u8>>>;
}

/// Sized handle stored in the service store for dynamic services.
///
/// Store values are `Arc<dyn Any>`; trait objects cannot be downcast
/// directly, so dynamic services are provided as [`Invoker`] wrappers and
/// consumers read them with `ctx.get::<Invoker>(name)`.
#[derive(Clone)]
pub struct Invoker {
    /// The boundary adapter implementing the call channel.
    pub inner: Arc<dyn Invoke>,
}

impl Invoker {
    /// Wraps an implementation into a storable handle.
    #[must_use]
    pub fn new(inner: Arc<dyn Invoke>) -> Self {
        Self { inner }
    }

    /// Forwards one method invocation to the implementation.
    ///
    /// # Errors
    ///
    /// Propagates the implementation's failure.
    pub fn call(&self, method: u32, payload: Vec<u8>) -> BoxFuture<Result<Vec<u8>>> {
        self.inner.call(method, payload)
    }
}

impl std::fmt::Debug for Invoker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invoker").finish_non_exhaustive()
    }
}
