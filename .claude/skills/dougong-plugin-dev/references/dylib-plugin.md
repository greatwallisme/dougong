# Dylib plugins (C-ABI cdylib)

A dylib plugin is a `cdylib` crate depending **only** on `dougong-abi` - no
kernel, no tokio, no serde crosses the boundary. All framework semantics run
host-side ("inverted API"): the plugin only computes. Reference
implementation: `crates/greeter-plugin`.

Trust model: dylibs are **trusted** code (no sandbox). A host keeps every
plugin mapped for the process lifetime - teardown-only, never `dlclose`
(design decision D5, user ruling R1). Design plugin-private state for the
process lifetime; `teardown` releases *state*, not the library.

## Crate shape

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
dougong-abi = { path = "../dougong-abi" }
```

Join the workspace `members` list. `cargo build --workspace` produces
`target/debug/lib<crate_name>.so` (the real artifact loaders consume).

## The export table

The host resolves one symbol by name at load time and rejects the plugin on
ABI mismatch:

```rust
use dougong_abi::{ABI_VERSION, PluginExports, write_out};

#[unsafe(no_mangle)]
pub static DOUGONG_PLUGIN_EXPORTS: PluginExports = PluginExports {
    abi_version: ABI_VERSION,   // host rejects a different value
    name,                       // display name -> out buffer
    descriptor,                 // UTF-8 JSON descriptor -> out buffer
    setup,                      // -> opaque state handle (null = init failure)
    call,                       // serve one provided-service method
    on_event,                   // receive one dispatched event
    on_next,                    // deliver a deferred waterfall continuation
    teardown,                   // release the setup handle (exactly once)
};
```

`ABI_VERSION` is 2 (v2 = CPS event protocol, D14). Any change to
`PluginExports` or the constants breaks the ABI: bump it and update every
duplicate table in the same commit.

## Descriptor

`descriptor` writes UTF-8 JSON declaring the plugin - this is what the
kernel turns into `inject`/`provide` requirements and `listen` event
registrations, so it must match what `call`/`on_event` actually serve:

```rust
unsafe extern "C" fn descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    let json = br#"{"name":"greeter","version":"0.2.0","provide":["greeting"],
"inject":[],"listen":{"ping":1,"upper":2}}"#;
    if unsafe { write_out(json, out, cap, len) } { dougong_abi::STATUS_OK }
    else { dougong_abi::STATUS_CAPACITY }
}
```

`listen` maps event names to the u32 event ids your `on_event` switch
handles. Malformed entries (non-object, non-u32) fail the load. Provide only
what `call` serves; the host rejects undeclared provides.

## The buffer protocol (all callbacks)

Caller-owned buffers: write at most `cap` bytes into `out_buf`, store the
written length in `out_len`, return `STATUS_CAPACITY` when it does not fit -
the host retries with a doubled buffer (bounded at 16 MiB). Always use
`dougong_abi::write_out` - never hand-roll the pointer math. Input `in_data`
is valid for `in_len` readable bytes. The plugin never allocates anything
the host must free, and vice versa.

## State, call, teardown

```rust
struct State { invocations: u32 }

unsafe extern "C" fn setup() -> *mut std::os::raw::c_void {
    Box::into_raw(Box::new(State { invocations: 0 })).cast()  // null = failure
}

unsafe extern "C" fn teardown(handle: *mut std::os::raw::c_void) {
    if handle.is_null() { return; }
    drop(unsafe { Box::from_raw(handle.cast::<State>()) });  // exactly once
}
```

- **Method selectors are `u32` constants** of the service contract (dynamic
  channel D6). Payload encoding is method-specific bytes (JSON for kernel
  values).
- The host serializes every foreign call that shares one handle through a
  per-instance lock (service calls, events, deferred continuations,
  teardown), so plain `&mut State` needs no internal locking even with
  several consumer fibers calling concurrently.

## Events (v2 CPS protocol)

`on_event` receives `(handle, event: u32, mode: u32, data, len,
next_available: u32, out, cap, out_len) -> i32`. `mode` is one of
`MODE_EMIT(0) / MODE_PARALLEL(1) / MODE_SERIAL(2) / MODE_BAIL(3) /
MODE_WATERFALL(4)` - chosen at dispatch by the host, never at registration.

- Plain reply: write the JSON-encoded reply and return `STATUS_OK`
  (bare strings need quotes: `"\"pong\""`). `STATUS_BAILED` marks the output
  as a bail value for serial/bail chains.
- **Waterfall pull**: when `next_available == 1`, return
  `STATUS_DEFERRED` to request the rest of the chain. Output written before
  deferring is the payload the chain continues with (empty = the original
  payload). The host resolves the continuation asynchronously and re-enters:
- `on_next` receives `(handle, event, chain_status, data, len, out, cap,
  out_len)`: `STATUS_OK` means `data`/`len` carry the rest-of-chain result -
  post-process it (e.g. transform, upper-case) and write your final reply,
  returning `STATUS_OK`; `STATUS_FAILED` means the chain errored. Returning
  `STATUS_DEFERRED` from `on_next` is a contract violation.
- Returning anything else from `on_event` (no defer) **vetoes** the
  waterfall chain: your output becomes the chain value at this listener.
- Track the deferred state in your `State` (e.g. a `pending` flag): the
  host guarantees no other foreign call interleaves between your deferred
  `on_event` and its `on_next`.

- **Teardown must never fail the process** - best-effort stderr writes only.
  The host calls it exactly once per successful `setup`, after all disposers
  referencing the handle drained.

## Loading

Via the loader document (`dougong-loader`) or directly:

```rust
let plugin = dougong_dylib::load(path)?;  // -> Arc<dyn Plugin>
```

```json
{ "name": "greeter", "source": { "kind": "dylib", "path": "./libgreeter_plugin.so" },
  "config": { "greeting": "hi" }, "isolate": null }
```

`config` reaches the plugin through the host's activation flow; validate it
where the plugin consumes it. See `kernel-model.md` for the loader's
reconcile semantics.

## Verification

```sh
cargo build -p my-plugin                          # produces the .so
nm -D target/debug/libmy_plugin.so | rg DOUGONG_PLUGIN_EXPORTS
cargo clippy -p my-plugin --all-targets -- -D warnings
cargo run -p dougong-loader --example loader_demo # end-to-end (uses the greeter artifact)
```

## Pitfalls

- `unsafe` blocks need SAFETY comments on both sides of each FFI assumption
  (host buffer contract, handle ownership) - the workspace enforces it.
- Buffer math: respect `cap`, always set `out_len`, `STATUS_CAPACITY` on
  overflow (the host retries). Overrunning is UB.
- Descriptor must declare every service `call` serves and every event
  `on_event` switches on - undeclared provides fail the kernel capability
  check host-side; unregistered events never dispatch.
- DEFERRED outside waterfall, or `STATUS_DEFERRED` from `on_next`, is a
  contract violation the host rejects.
- Keep only `dougong-abi` as a dependency; pulling in the kernel or tokio
  across artifacts duplicates runtime state and breaks the portability
  contract (`#[repr(C)]` only, no `TypeId`/statics crossing).
