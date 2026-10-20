# Kernel model and in-process Rust plugins

This is the reference semantics every dougong plugin form inherits. Read it
when a task touches provide/inject resolution, effects, events, epochs, or
the loader. Code examples use the in-process API (`dougong` crate) - the
canonical way to learn the model.

## The Plugin trait

```rust
use std::borrow::Cow;
use dougong::{plugin, BoxFuture, Injection, Plugin, Result};
use serde_json::Value;

struct Clock;

impl Plugin for Clock {
    fn name(&self) -> Cow<'static, str> { Cow::Borrowed("clock") }

    // Declared dependencies: the fiber only activates once ALL are
    // available, and re-activates when the provider set changes.
    fn inject(&self) -> Vec<Injection> {
        vec![Injection::new("time.source")]
    }

    fn apply(&self, ctx: dougong::Context, config: Value) -> BoxFuture<Result<()>> {
        Box::pin(async move {
            // Config is a serde_json::Value - validate at the top.
            // All registration happens here; see "Effects" below.
            Ok(())
        })
    }
}

// Or the closure form:
let clock = plugin("clock", vec![Injection::new("time.source")], |ctx, _config| {
    Box::pin(async move { Ok(()) })
});
```

## Activation

```rust
let handle = ctx.plugin(plugin_object, Some(config_value))?;  // child fiber
handle.join().await?;                                          // await load
handle.update(new_config)?;                                    // hot update (may veto via internal/update)
ctx.uninstall(&plugin_object).await;                           // LIFO teardown + drain
```

A plugin started inside another plugin's fiber becomes an *effect of the
parent*: unloading the parent cascades to the child. The top-level entry is
`ctx.inject(deps, callback)`, which starts an anonymous root fiber once the
named services are available.

## Lifecycle and epochs

Each plugin instance is a **fiber** with a six-state lifecycle. The key
property: the *committed view* (committed store + provider candidates) is
committed **before** the body runs, so a provider can read its own service
during activation and late consumers never stay Pending unnecessarily.
Consumers whose declared deps are absent park in `Pending`; when every dep
becomes available the fiber converges on the *epoch* - the string of provider
uids - and re-activates whenever that set changes. `EPOCH_INACTIVE` marks the
not-loaded state; a restart is forced only when leaving it.

## Services: typed store and the two-level check

```rust
// Provider side (revertible - withdrawal removes the service):
ctx.provide("time.source", MySource::new()).await?;
ctx.set("time.source", updated)?;                 // overwrite own service
ctx.accessor("time.source", |ctx| compute(ctx)).await?;  // re-evaluated per read

// Consumer side - typed downcast through Arc<dyn Any>:
let src: Arc<MySource> = ctx.require("time.source")?;   // Result
let maybe: Option<Arc<MySource>> = ctx.get("time.source");
```

`require`/`get` implement the two-level capability check (TS Algorithm 6):

- `undeclared_dependency` - the fiber never declared the service in
  `inject()`. This is a **capability violation**, load-bearing for boundary
  error mapping (WIT `kernel-error::undeclared-dependency`).
- `inactive_dependency` - declared but not committed yet. The fiber parks in
  `Pending` instead; this error surfaces only on direct synchronous reads.

Per-service config layers on the fiber scope via `ctx.intercept(name,
config)` and reads back through `ctx.service_config(name)`.

## Effects: teardown is derived from setup

Every environmental mutation in a plugin path goes through effect
collection; disposers unwind LIFO when the fiber is withdrawn. The helpers
above are thin wrappers; for anything custom:

```rust
ctx.effect("open-file", async {
    let file = open(...)?;
    Ok(Some(Disposer::new("close-file", move |_ctx| async move {
        // cleanup; runs LIFO on withdrawal
    })))
}).await?;
```

To run code *last* at teardown, register it *first* in `apply` - LIFO does
the rest. This is the same invariant the dylib/wasm adapters encode by
registering the teardown effect before anything else.

Provider withdrawal additionally **drains consumers first**: a service's
disposer joins all consumer fibers that relied on it (reverse-dependency
index) before the provider's own effects unwind. Never depend on a service
outliving your fiber's disposal.

## Events: five modes at dispatch time

```rust
// Registration (mode-agnostic):
ctx.on("deploy", listener).await?;       // scope-filtered
ctx.on_global("deploy", listener).await?;// bypasses scope filtering
ctx.once("deploy", listener).await?;     // auto-removed after one dispatch

// Dispatch - the mode is chosen HERE:
ctx.emit("deploy", payload);                       // fire-and-forget
let v = ctx.parallel("deploy", payload).await?;    // all listeners concurrently
let v = ctx.serial("deploy", payload).await?;      // in order, stop on bail
let v = ctx.bail("deploy", payload).await?;        // no awaiting
let v = ctx.waterfall("deploy", payload, fallback).await?; // chain + fallback
```

A listener is
`Arc<dyn Fn(Context, Value, Next) -> BoxFuture<Result<Value>> + Send + Sync>`.
In waterfall, `next.call(transformed_payload).await?` advances the chain;
**dropping the `Next` without calling vetoes** the rest of the chain and the
fallback. `dougong::is_bailed(&value)` checks a bail marker. `internal/*`
event names are framework extension points with scope filtering - never
dispatch user traffic under that prefix.

## Isolation scopes

`ctx.isolate(name)` mints a **fresh label on every call** - two realms that
must share a scope need `ctx.isolate_with_label(name, label)` with a
caller-held mapping allocated via `ctx.fresh_label()` (see `Reconciler`'s
`scopes` map for the reference pattern).

## Logging

`ctx.logger() -> Arc<dyn Logger>` resolves the `logger` service with a
stderr fallback. Library and example code log through it - never
`println!`/`eprintln!`.

## Loading declaratively (dougong-loader)

One JSON document describes the full desired state; `Reconciler::apply` diffs
it against live fibers (insert / update / retire; scope change = retire +
re-insert) and is idempotent - a second apply over an unchanged document
reports `converged()`.

```json
{
  "plugins": [
    { "name": "greeter",
      "source": { "kind": "dylib", "path": "./libgreeter_plugin.so" },
      "config": { "greeting": "hi" },
      "isolate": null }
  ]
}
```

`source.kind` is `dylib`, `wasm`, or `builtin` (`{"kind": "builtin",
"name": "<registry key>"}` resolves from the `HashMap` handed to
`Reconciler::new(builtins, wasm_host_option)`). Requirements (`inject` /
`provide`) come from each artifact's own descriptor - the document must not
declare them. Per-slot failures land in `Report::failures`; one broken
artifact never blocks the rest. File watching is out of scope: any trigger
feeds `replace` + `apply`.

## Error model

`dougong::Error::new(kind, msg)`; kinds you will meet: `invalid_config`,
the two dependency errors above, capability denials, `fail_load` (a panicking
body is contained - the body runs on its own task and a panic becomes load
failure, never a driver crash).
