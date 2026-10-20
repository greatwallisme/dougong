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
    teardown,                   // release the setup handle (exactly once)
};
```

`ABI_VERSION` is 1. Any change to `PluginExports` or the constants breaks the
ABI: bump it and update every duplicate table in the same commit.

## Descriptor

`descriptor` writes UTF-8 JSON declaring the plugin - this is what the
kernel turns into `inject`/`provide` requirements, so it must match what
`call` actually serves:

```rust
unsafe extern "C" fn descriptor(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    let json = br#"{"name":"greeter","version":"0.1.0","provide":["greeting"],"inject":[]}"#;
    if unsafe { write_out(json, out, cap, len) } { dougong_abi::STATUS_OK }
    else { dougong_abi::STATUS_CAPACITY }
}
```

## The buffer protocol (all callbacks)

Caller-owned buffers: write at most `cap` bytes into `out_buf`, store the
written length in `out_len`, return `STATUS_CAPACITY` when it does not fit.
Always use `dougong_abi::write_out` - never hand-roll the pointer math. Input
`in_data` is valid for `in_len` readable bytes. The plugin never allocates
anything the host must free, and vice versa.

## State, call, events, teardown

```rust
struct State { invocations: u32 }

unsafe extern "C" fn setup() -> *mut std::os::raw::c_void {
    Box::into_raw(Box::new(State { invocations: 0 })).cast()  // null = failure
}

unsafe extern "C" fn call(
    handle: *mut std::os::raw::c_void,
    method: u32,
    _data: *const u8, _len: usize,
    out: *mut u8, cap: usize, out_len: *mut usize,
) -> i32 {
    if handle.is_null() { return dougong_abi::STATUS_FAILED; }
    let state = unsafe { &mut *(handle.cast::<State>()) };
    if method != GREETER_HELLO { return dougong_abi::STATUS_FAILED; }
    state.invocations += 1;
    if unsafe { write_out(REPLY, out, cap, out_len) } { dougong_abi::STATUS_OK }
    else { dougong_abi::STATUS_CAPACITY }
}

unsafe extern "C" fn teardown(handle: *mut std::os::raw::c_void) {
    if handle.is_null() { return; }
    drop(unsafe { Box::from_raw(handle.cast::<State>()) });  // exactly once
}
```

- **Method selectors are `u32` constants** of the service contract (dynamic
  channel D6). Define and document them in the crate; payload encoding is
  method-specific bytes (JSON for kernel values).
- **Events**: `on_event` receives `(handle, event: u32, mode: u32, data, len,
  next: Option<NextFn>, next_ctx, out, cap, out_len)`. `mode` is one of
  `MODE_EMIT(0) / MODE_PARALLEL(1) / MODE_SERIAL(2) / MODE_BAIL(3) /
  MODE_WATERFALL(4)` - chosen at dispatch by the host. Return
  `STATUS_BAILED` to stop a serial/bail chain (output carries the bail
  value). For waterfall, `next` is `Some`: call it through `next_ctx` to
  advance the chain (transformed payload in, rest-of-chain result out);
  never calling it vetoes the chain.
- **Teardown must never fail the process** - best-effort stderr writes only.
  The host calls it exactly once per successful `setup`, after all disposers
  referencing the handle drained.

## Loading

Via the loader document (`dougong-loader`) or directly:

```rust
let plugin = dougong_dylib::DynPlugin::load(path)?;  // -> Arc<dyn Plugin>
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
  overflow. Overrunning is UB.
- Descriptor must declare every service `call` serves - undeclared provides
  fail the kernel capability check host-side.
- Keep only `dougong-abi` as a dependency; pulling in the kernel or tokio
  across artifacts duplicates runtime state and breaks the portability
  contract (`#[repr(C)]` only, no `TypeId`/statics crossing).
