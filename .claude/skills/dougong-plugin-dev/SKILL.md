---
name: dougong-plugin-dev
description: Develop plugins for the dougong composition kernel (this repository) - in-process Rust plugins, C-ABI dylib plugins, and sandboxed wasm component plugins. Use whenever creating, modifying, loading, or debugging dougong plugins of any form - service providers or consumers, event listeners, teardown/disposer logic, loader JSON configs, or WIT contract changes. Trigger on "add a plugin", "provide a service", "inject a dependency", "register a listener", "fiber stuck Pending", or any work touching crates/greeter-plugin, crates/greeter-wasm, dougong-abi, dougong-dylib, dougong-wasm, dougong-loader, or wit/.
---

# Dougong plugin development

dougong is a cordis-style composition kernel: **time composability** (every
environmental mutation is a revertible effect, undone LIFO on withdrawal)
meets **spatial composability** (declared dependencies resolve reactively as
providers come and go). A plugin never hand-writes teardown - it declares and
registers, and the kernel derives reversal from registration.

All plugin forms implement **one semantic contract**. Version anchors for the
current code: ABI_VERSION 2 (`dougong-abi`, v2 CPS event protocol per D14),
WIT world `dougong:kernel@0.1.0` (`wit/dougong-plugin.wit`), contract hash
`dougong-dynamic-channel-v1`, wasmtime 48 / wit-bindgen 0.61, toolchain pinned
in `rust-toolchain.toml`.

## 1. Choose the plugin form

| Form | Trust | Boundary | Crate deps | Use when |
|------|-------|----------|------------|----------|
| In-process Rust | trusted, same process | Rust trait `dougong::Plugin` | `dougong` | ships inside the host binary; full typed API |
| dylib (`cdylib`) | trusted, separate artifact | C-ABI export table (`dougong-abi`) | `dougong-abi` only | independently compiled artifact, same trust domain |
| wasm component | untrusted, sandboxed | WIT world `dougong:kernel@0.1.0` | `wit-bindgen` only | third-party or fault-isolated code; fuel/epoch/memory capped |

Reference implementations to copy structure from (not names):
`crates/greeter-plugin` (dylib), `crates/greeter-wasm` (wasm),
`crates/dougong/src/lib.rs` doctest (in-process).

## 2. The contract every form implements

Five obligations, same shape in all three forms:

1. **Declare** - an inject list (consumed services) and a provide list.
   The kernel checks declarations at call time: consuming a service that was
   never declared is `undeclared_dependency` (capability error); declared but
   not yet committed is `inactive_dependency` and the consumer fiber parks in
   `Pending` until the provider commits (TS Algorithm 6 two-level check).
   Keep this distinction when mapping errors across a boundary.
2. **Activate** - the body (`apply`) receives the activation site and config.
   ALL registration happens here: `provide`, listeners, custom effects. The
   committed view (already-provided services) is visible *before* the body
   runs, so a provider may read its own service during activation.
3. **Serve** - cross-boundary services use the dynamic channel: service name
   + `method: u32` selector + opaque bytes. The boundary contract hash is
   `dougong-dynamic-channel-v1`. Payload encoding is JSON for kernel values.
4. **Listen** - five dispatch modes (emit / parallel / serial / bail /
   waterfall) are chosen **at dispatch time**, never at registration. In
   waterfall, a listener that drops the `Next` token vetoes the rest of the
   chain. `internal/*` events are framework extension points with scope
   filtering - do not collide with that prefix.
5. **Dispose** - the kernel reverses registrations LIFO on withdrawal, and a
   provider's consumers are drained *before* the provider's effects unwind
   (drain protocol). Handle `0` (wasm `run-disposer`, dylib `teardown` export)
   is the plugin teardown. To run code *last* at teardown, register it
   **first** in `apply` - LIFO ordering does the rest.

Config arrives as `serde_json::Value` (in-process) or JSON bytes
(dylib descriptor / wasm `apply`); validate it at the top of the body.

## 3. References - read the one for your form

| Reference | Read when |
|-----------|-----------|
| [kernel-model.md](references/kernel-model.md) | Writing in-process Rust plugins; need the Context API, event modes, effect collection, error model, or loader config format. Explains the shared semantics all forms inherit. |
| [dylib-plugin.md](references/dylib-plugin.md) | Building a `cdylib` plugin: export table, JSON descriptor, buffer protocol, status codes, state and teardown. |
| [wasm-plugin.md](references/wasm-plugin.md) | Building a wasm component: crate layout, WIT bindings, Guest impl, JSON payload rules, sandbox expectations, host-side requirements. |

## 4. Verification

```sh
cargo build --workspace                                 # host build (all forms)
cargo build -p greeter-wasm --target wasm32-wasip2      # wasm guest artifact
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings, always
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# End-to-end proof per form (each must exit 0):
cargo run -p dougong-dylib  --example dylib_demo    # dylib artifact, needs cargo build --workspace first
cargo run -p dougong-wasm   --example wasm_demo     # wasm artifact, needs the wasip2 build above
cargo run -p dougong-loader --example loader_demo   # loader lifecycle over the dylib artifact
```

A new plugin crate must join the workspace (`members` in the root
`Cargo.toml`) and keep ALL four gates green. Wasm guest crates stay
`#[cfg(target_arch = "wasm32")]`-gated so host-target workspace builds
compile them as empty libraries - see [wasm-plugin.md](references/wasm-plugin.md).

## 5. Pitfall index (details in the references)

- Service never resolves / fiber stays `Pending`: the consumer did not
  declare `inject`, or the provider never committed. `undeclared_dependency`
  vs `inactive_dependency` tells you which half failed.
- Wasm event listener never fires: registration happens in `apply`, not at
  export time - the guest must call `register_listener` during `apply`.
- Wasm replies must be **valid JSON bytes** - a bare string needs quotes
  (`"\"pong\""`), or the host-side decode fails.
- Wasm host: a `wasm32-wasip2` guest built with std always declares
  `wasi:io` imports - the host must link `wasmtime-wasi` even for
  capability-empty sandboxes.
- Dylib: honor the caller-owned buffer protocol via `dougong_abi::write_out`;
  return `STATUS_CAPACITY` instead of overrunning.
- Dylibs are teardown-only: a host never `dlclose`s a plugin (D5/R1) - design
  plugin state for the process lifetime.
- Teardown code that must run last goes first in `apply` (LIFO reversal).
