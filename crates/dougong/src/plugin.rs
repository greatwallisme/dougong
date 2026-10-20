// Rust guideline compliant 2026-10-20
//! Plugin trait, dependency declarations, and constructor helpers.

use std::borrow::Cow;
use std::sync::Arc;

use serde_json::Value;

use crate::context::Context;
use crate::error::Result;

/// A boxed future returned by plugin bodies.
pub type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// One declared service dependency of a plugin.
///
/// `intercept` carries optional intercept config that is layered onto the
/// plugin context for that service while the fiber is loaded (TS merges the
/// inject map into the fiber's intercept chain).
#[derive(Debug, Clone)]
pub struct Injection {
    /// Required service name.
    pub service: Cow<'static, str>,
    /// Optional intercept config applied for this service on the fiber scope.
    pub intercept: Option<Value>,
}

impl Injection {
    /// Declares a dependency without intercept config.
    #[must_use]
    pub fn new(service: impl Into<Cow<'static, str>>) -> Self {
        Self {
            service: service.into(),
            intercept: None,
        }
    }

    /// Declares a dependency with intercept config.
    #[must_use]
    pub fn with_intercept(service: impl Into<Cow<'static, str>>, intercept: Value) -> Self {
        Self {
            service: service.into(),
            intercept: Some(intercept),
        }
    }
}

impl From<&'static str> for Injection {
    fn from(service: &'static str) -> Self {
        Self::new(service)
    }
}

/// The unit of composition: declared requirements plus an activation body.
///
/// Every environmental mutation performed inside [`Plugin::apply`] must go
/// through [`Context::effect`] (or helpers built on it such as
/// [`Context::provide`]), so teardown is derived from setup rather than
/// hand-written (paper Eq. 12/37, TS `ctx.effect`).
///
/// Config arrives as [`Value`]; validate/normalize at the top of `apply`.
pub trait Plugin: Send + Sync + 'static {
    /// Display name used for diagnostics and logger names.
    fn name(&self) -> Cow<'static, str>;

    /// Services this plugin requires; the fiber only activates when all are
    /// available, and re-activates whenever the provider set changes.
    fn inject(&self) -> Vec<Injection> {
        Vec::new()
    }

    /// Runs the plugin body. Return an error to fail activation; collected
    /// effects are rolled back LIFO.
    fn apply(&self, ctx: Context, config: Value) -> BoxFuture<Result<()>>;
}

/// Builds a plugin from a name, dependency list, and an async closure.
///
/// This is the direct analogue of cordis `plugin_with` / `ctx.inject`.
pub fn plugin<F>(name: &'static str, inject: Vec<Injection>, body: F) -> Arc<dyn Plugin>
where
    F: Fn(Context, Value) -> BoxFuture<Result<()>> + Send + Sync + 'static,
{
    Arc::new(ClosurePlugin { name, inject, body })
}

struct ClosurePlugin<F> {
    name: &'static str,
    inject: Vec<Injection>,
    body: F,
}

impl<F> Plugin for ClosurePlugin<F>
where
    F: Fn(Context, Value) -> BoxFuture<Result<()>> + Send + Sync + 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.name)
    }

    fn inject(&self) -> Vec<Injection> {
        self.inject.clone()
    }

    fn apply(&self, ctx: Context, config: Value) -> BoxFuture<Result<()>> {
        (self.body)(ctx, config)
    }
}

pub struct AnonymousPlugin<F> {
    pub inject: Vec<Injection>,
    pub body: F,
}

impl<F> Plugin for AnonymousPlugin<F>
where
    F: Fn(Context) -> BoxFuture<Result<()>> + Send + Sync + 'static,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("anonymous")
    }

    fn inject(&self) -> Vec<Injection> {
        self.inject.clone()
    }

    fn apply(&self, ctx: Context, _config: Value) -> BoxFuture<Result<()>> {
        (self.body)(ctx)
    }
}
