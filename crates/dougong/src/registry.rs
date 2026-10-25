// Rust guideline compliant 2026-10-20
//! Plugin registry: identity-keyed runtime records and fiber spawning.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use serde_json::Value;

use crate::context::Context;
use crate::error::{Error, Result};
use crate::fiber::{Fiber, FiberHandle, Runtime};
use crate::invariant;
use crate::plugin::{AnonymousPlugin, Injection, Plugin};
use crate::scope::ScopeNode;

/// Registry map: plugin identity key -> shared runtime record.
pub type RegistryMap = HashMap<u64, Arc<Runtime>>;

/// Identity key of a plugin handle: the allocation address of the `Arc`.
///
/// Two `ctx.plugin()` calls with the same `Arc<dyn Plugin>` share one runtime
/// record (cordis keys runtimes by the plugin callback identity).
pub fn plugin_key(plugin: &Arc<dyn Plugin>) -> u64 {
    Arc::as_ptr(plugin).cast::<()>() as u64
}

/// Starts a plugin in `ctx`, registering the child fiber as an effect on the
/// parent fiber so unloading the parent cascades (decision Q3/D, paper
/// Eq. 31).
pub fn plugin(
    ctx: &Context,
    plugin: Arc<dyn Plugin>,
    config: Option<Value>,
) -> Result<FiberHandle> {
    let key = plugin_key(&plugin);
    let runtime = {
        let mut registry = ctx.root.registry.lock();
        Arc::clone(registry.entry(key).or_insert_with(|| {
            Arc::new(Runtime {
                key,
                name: Some(plugin.name().to_string()),
                plugin,
                fibers: Mutex::new(Vec::new()),
            })
        }))
    };

    let uid = ctx.root.next_uid();
    let parent_fiber = ctx.fiber().ok_or_else(Error::inactive_effect)?;
    parent_fiber.assert_active()?;

    let inject = plugin_runtime_inject(&runtime);
    // Inject-declared intercept config layers onto the child scope (TS merges
    // the inject map into the fiber's intercept chain).
    let scope = child_scope(&ctx.scope, &inject);

    let child = Fiber::new_child(
        Arc::clone(&runtime),
        inject,
        config.unwrap_or(Value::Null),
        uid,
        &parent_fiber,
        Arc::clone(&scope),
    );
    {
        let mut fibers = runtime.fibers.lock();
        fibers.push(Arc::downgrade(&child));
    };
    let child_ctx = Context {
        root: Arc::clone(&ctx.root),
        scope,
        fiber: Arc::downgrade(&child),
    };
    child.bind(&child_ctx);
    ctx.root.register_fiber(&child);
    ctx.root.index.add_interest(uid, &child.inject_names());
    // Before the cascade effect exists, so no dispose can race the checks.
    invariant::post_spawn(&child);

    // O4: announce the fiber's existence once the spawn invariant accepted
    // it; the state family covers transitions, this covers birth.
    let payload = crate::events::system_event(Some(child.as_ref()), &[]);
    crate::events::emit_internal(&child_ctx, "internal/fiber.created", &payload, None);

    // Cascade: the child's whole lifecycle is one effect of the parent
    // (paper Eq. 31). Registered synchronously; a parent that cannot host the
    // child fails the spawn, and the just-created fiber is disposed in the
    // background (TS: `void Promise.resolve(dispose()).catch(log)`).
    {
        let cascade_child = Arc::clone(&child);
        let label = format!("dougong.plugin({})", child.name());
        let disposer: crate::fiber::Disposer = Box::new(move || {
            Box::pin(async move {
                cascade_child.dispose().await;
            })
        });
        if let Err(err) = parent_fiber.register_effect(
            label,
            // The cascade is kernel-structural: a child dispose must never
            // be orphaned by the parent's cleanup budget (S3).
            crate::fiber::Teardown::Critical,
            Some(disposer),
        ) {
            tokio::spawn(async move {
                child.dispose().await;
            });
            return Err(err);
        }
    }

    // Initial dependency resolution: publish checks every declared injection
    // BEFORE the first refresh (TS `_checkImpl` loop at publication). Without
    // this, a consumer spawned after its provider never sees the existing
    // implementation.
    for name in child.inject_names() {
        child.check_impl(&name);
    }
    child.refresh();
    Ok(FiberHandle(child))
}

fn plugin_runtime_inject(runtime: &Arc<Runtime>) -> Vec<Injection> {
    runtime.plugin.inject()
}

fn child_scope(base: &Arc<ScopeNode>, inject: &[Injection]) -> Arc<ScopeNode> {
    let mut scope = Arc::clone(base);
    for injection in inject {
        if let Some(config) = &injection.intercept {
            scope = ScopeNode::intercepted(&scope, injection.service.as_ref(), config.clone());
        }
    }
    scope
}

/// Removes a disposed fiber from its runtime record.
pub fn remove_from_runtime(fiber: &Fiber) {
    if let Some(runtime) = &fiber.runtime {
        let mut fibers = runtime.fibers.lock();
        fibers.retain(|weak| {
            weak.upgrade()
                .is_some_and(|strong| !std::ptr::eq(&raw const *strong, fiber))
        });
    }
}

/// Starts an anonymous callback once the named services are available.
///
/// # Errors
///
/// Fails when the parent fiber cannot accept the cascade effect.
pub fn inject<F>(ctx: &Context, deps: Vec<Injection>, callback: F) -> Result<FiberHandle>
where
    F: Fn(Context) -> crate::plugin::BoxFuture<crate::error::Result<()>> + Send + Sync + 'static,
{
    let anon: Arc<dyn Plugin> = Arc::new(AnonymousPlugin {
        inject: deps,
        body: callback,
    });
    plugin(ctx, anon, None)
}

/// Looks up the runtime for a plugin handle, if registered.
#[must_use]
pub fn get(root_registry: &Mutex<RegistryMap>, plugin: &Arc<dyn Plugin>) -> Option<Arc<Runtime>> {
    let key = plugin_key(plugin);
    root_registry.lock().get(&key).map(Arc::clone)
}

/// Disposes every live fiber of a plugin and removes its runtime record.
pub async fn delete(ctx: &Context, plugin: &Arc<dyn Plugin>) -> Option<Arc<Runtime>> {
    let key = plugin_key(plugin);
    let runtime = {
        let mut registry = ctx.root.registry.lock();
        registry.remove(&key)
    }?;
    let fibers: Vec<Arc<Fiber>> = runtime
        .fibers
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    for fiber in fibers {
        fiber.dispose().await;
    }
    Some(runtime)
}

/// Number of registered plugin runtimes.
#[must_use]
pub fn size(registry: &Mutex<RegistryMap>) -> usize {
    registry.lock().len()
}

/// True when the plugin has at least one live fiber.
#[must_use]
pub fn has(registry: &Mutex<RegistryMap>, plugin: &Arc<dyn Plugin>) -> bool {
    let key = plugin_key(plugin);
    registry
        .lock()
        .get(&key)
        .is_some_and(|runtime| !runtime.fibers.lock().is_empty())
}
