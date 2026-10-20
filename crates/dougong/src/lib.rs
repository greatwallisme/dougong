// Rust guideline compliant 2026-10-20
//! dougong: a cordis-style composition kernel in Rust.
//!
//! Time composability (every mutation is a revertible effect) meets spatial
//! composability (dependencies are declared, resolved, and re-resolved as
//! providers come and go). See `doc/research/` for the full design rationale.
//!
//! # Core handles
//!
//! - [`Context`]: cheap-clonable dependency container handle.
//! - [`Fiber`] / [`FiberHandle`]: one live plugin instance.
//! - [`Plugin`]: requirements + activation body; effects go through
//!   [`Context::effect`] so teardown is derived from setup.
//!
//! # Example
//!
//! ```
//! use dougong::{plugin, Context, Injection};
//!
//! # async fn demo() -> dougong::Result<()> {
//! let ctx = Context::new();
//!
//! let provider = plugin("provider", vec![], |ctx, _config| Box::pin(async move {
//!     ctx.provide("greeting", "hello".to_string()).await?;
//!     Ok(())
//! }));
//! let consumer = plugin(
//!     "consumer",
//!     vec![Injection::new("greeting")],
//!     |ctx, _config| Box::pin(async move {
//!         let greeting = ctx.require::<String>("greeting")?;
//!         assert_eq!(&*greeting, "hello");
//!         Ok(())
//!     }),
//! );
//!
//! let consumer_fiber = ctx.inject(vec![], move |ctx| {
//!     let provider = std::sync::Arc::clone(&provider);
//!     let consumer = std::sync::Arc::clone(&consumer);
//!     Box::pin(async move {
//!         ctx.plugin(provider, None)?.join().await?;
//!         ctx.plugin(consumer, None)?.join().await?;
//!         Ok(())
//!     })
//! })?;
//! consumer_fiber.join().await?;
//! # Ok(())
//! # }
//! ```

mod context;
mod error;
mod events;
mod fiber;
mod invoke;
mod logger;
mod plugin;
mod registry;
mod scope;
mod service;

pub use context::Context;
pub use error::{Error, Result};
pub use events::{Listener, Next, is_bailed};
pub use fiber::{
    Disposer, EPOCH_INACTIVE, EffectGuard, Fiber, FiberHandle, FiberState, ProviderSnapshot,
    Runtime,
};
pub use invoke::{Invoke, Invoker};
pub use logger::{LogLevel, Logger, LoggerHandle, StderrLogger};
pub use plugin::{BoxFuture, Injection, Plugin, plugin};
pub use scope::{DEFAULT_LABEL, ScopeNode};
