# Wasm component plugins (sandboxed)

A wasm plugin is a Component Model component depending **only** on
`wit-bindgen` and the WIT contract at `wit/dougong-plugin.wit` (world
`dougong:kernel@0.1.0`). The kernel keeps all framework semantics; the
component only computes, inside a sandbox: fuel per export call (default
50M), epoch yielding (5 ms tick), `StoreLimits` (default 64 MiB memory), and
a capability-empty WASI context. Reference implementation:
`crates/greeter-wasm`.

Version anchors: wasmtime 48 / wit-bindgen 0.61 / `wasm32-wasip2`, plain
cargo (no cargo-component needed). The deeper API facts live in the
`wasmtime` and `wit` skills - consult them for host-embedding work.

## Crate shape (the gating is load-bearing)

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = { workspace = true }   # "0.61"
```

`src/lib.rs` - everything is behind `#[cfg(target_arch = "wasm32")]` so the
host-target workspace build compiles this crate to an empty library and all
gates stay green:

```rust
#[cfg(target_arch = "wasm32")]
#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        world: "dougong-plugin",
        path: "../../wit",   // relative to the crate manifest
    });
}

#[cfg(target_arch = "wasm32")]
mod imp;
#[cfg(target_arch = "wasm32")]
pub use imp::MyPlugin;
#[cfg(target_arch = "wasm32")]
bindings::export!(MyPlugin with_types_in bindings);
```

Generated bindings live in `mod bindings` - regenerate via the macro, never
hand-edit. Note `gen` is a reserved keyword in edition 2024 (name the module
`bindings`). Within `imp.rs`, imports resolve as `crate::bindings::...`.

## Build

```sh
rustup target add wasm32-wasip2                  # once (toolchain file lists it)
cargo build -p my-wasm --target wasm32-wasip2
# artifact: target/wasm32-wasip2/debug/my_wasm.wasm
```

## The Guest trait (world `dougong-plugin`)

Implement `bindings::exports::dougong::kernel::plugin::Guest`:

| Method | Role |
|--------|------|
| `descriptor() -> PluginDescriptor` | name, version, `inject: Vec<ServiceKey>`, `provide: Vec<ServiceKey>` |
| `apply(site: CallContext, config: Vec<u8>) -> Result<(), KernelError>` | the body; ALL registration happens here |
| `handle_service_call(key, method: u32, payload) -> Result<Vec<u8>, KernelError>` | serve one provided-service method |
| `handle_event(ev, listener: u64, payload, next: Option<u64>) -> Result<Vec<u8>, KernelError>` | one dispatched event |
| `run_disposer(handle: u64) -> Result<(), KernelError>` | collected disposers; `0` = plugin teardown |

Host imports (via `crate::bindings::dougong::kernel::host`): `log`,
`call_service`, `provide_service`, `register_listener`, `dispatch`,
`waterfall_next`, `effect_collect`.

## Keys, methods, payloads

- Service/event identity is `{name, abi-hash}`. The contract hash for the
  built-in dynamic channel is the byte string
  `dougong-dynamic-channel-v1` - stamp every key with it:

```rust
const ABI_HASH: &[u8] = b"dougong-dynamic-channel-v1";
const CALC_ADD: u32 = 0;        // method selectors are u32 constants
const LISTENER_PING: u64 = 1;   // listener ids are u64, guest-chosen

fn service_key(name: &str) -> ServiceKey {
    ServiceKey { name: name.to_string(), abi_hash: ABI_HASH.to_vec() }
}
```

- **Payloads are JSON-encoded kernel values in `Vec<u8>`**. A bare string
  must be quoted - `Ok(b"\"pong\"".to_vec())`, not `b"pong"`. Config bytes
  in `apply` decode as JSON (object or null).
- **Parsing without serde**: the reference plugins pull no dependencies
  beyond `wit-bindgen`, so payloads are parsed with hand-rolled extractors.
  Keep that pure logic in an *ungated* module (it compiles on the host
  target too) so its unit tests run in the normal `cargo test` suite - a
  test inside the wasm32-gated module never executes on the host. Adding
  `serde_json` as a guest dependency is also allowed; "only wit-bindgen"
  describes the reference footprint, not a rule.
- `handle_service_call` must verify `key.name` and `key.abi_hash`; a
  mismatch is `KernelError::CapabilityDenied`, an unknown method is
  `KernelError::Internal`.

## apply: registration ordering

Everything the kernel should reverse on withdrawal is registered inside
`apply`, in reverse-teardown order (the kernel unwinds LIFO):

```rust
fn apply(_site: CallContext, _config: Vec<u8>) -> Result<(), KernelError> {
    // host::provide_service(&service_key("calc.add"))?;      // provide
    // host::register_listener(&event_key("ping"), LISTENER_PING)?;  // listen
    // let handle = host::effect_collect("my.cleanup")?;       // custom effect
    Ok(())
}
```

- Provide only what `descriptor` declares - the host rejects undeclared
  provides. Likewise `call_service` fails with
  `undeclared-dependency` for services absent from `inject`.
- `provide_service` / `register_listener` / `effect_collect` each return a
  `registration` (or effect-handle) resource. Keep the `?` as a *statement*
  and end `apply` with `Ok(())` - a tail-position `expr?` makes `apply`
  return the resource instead of `()`. Dropping the resource withdraws
  nothing: reversal stays declarative, driven by fiber disposal.
- `effect_collect(label)` returns an `effect-handle` (from 1); the host later
  calls `run_disposer` with it. Handle `0` is the reserved plugin teardown.
- Guest state is a `thread_local!` `RefCell` - wasm execution is
  single-threaded and the host serializes every export call, so plain
  interior mutability suffices.

```rust
thread_local! {
    static STATE: RefCell<State> = RefCell::new(State { invocations: 0 });
}
```

## Events and waterfall

`handle_event` receives the listener id you registered plus the dispatch
mode's effects: `next` is `Some(token)` only in waterfall. Pull
`host::waterfall_next(token, payload)?` to advance the rest of the chain and
post-process its result; dropping without pulling vetoes the chain. Replies
are JSON bytes (quote bare strings). `internal/*` event names are framework
extension points - do not use that prefix.

## Sandbox expectations

The component can reach exactly the `host` interface and nothing else
(Component Model guarantee). Do not expect filesystem, network, clocks, or
threads; long compute is bounded by the per-call fuel budget - a trap there
surfaces host-side as a failed call, so keep export handlers bounded.

## Loading

```json
{ "name": "calc", "source": { "kind": "wasm", "path": "./calc_wasm.wasm" },
  "config": null, "isolate": null }
```

Host-side (embedding an app): create one shared
`dougong_wasm::WasmHost::new(WasmLimits::default())` inside a tokio runtime;
the host must link `wasmtime-wasi` even for capability-empty sandboxes - a
`wasm32-wasip2` guest built with std always declares `wasi:io` imports, and
instantiation fails without them ("matching implementation was not found").
One store = one execution: the adapter serializes guest calls; concurrent
callers wait and same-task reentry is rejected, so never assume reentrant
guest exports.

## Verification

```sh
cargo build -p my-wasm --target wasm32-wasip2        # the real artifact
cargo build --workspace                              # host target: empty lib, must stay green
cargo clippy -p my-wasm --all-targets -- -D warnings # host target, zero warnings
cargo run -p dougong-wasm --example wasm_demo        # end-to-end (greeter artifact)
```

## Pitfalls

- Event listener never fires: you returned from `apply` without calling
  `register_listener` - registration is runtime work in `apply`, not a
  declaration.
- Reply decode fails host-side: the reply was not valid JSON (bare string).
- Host build broke after adding the crate: the `cfg(target_arch = "wasm32")`
  gates are missing or a host-only dependency sneaked in.
- Instantiation fails with `wasi:io` errors: host-side problem - link
  `wasmtime-wasi`; also `StoreLimits::instances` must allow several core
  modules (wasip2 components instantiate more than one).
