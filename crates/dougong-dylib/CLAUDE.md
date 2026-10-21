# CLAUDE.md — dougong-dylib

dylib/so plugin host. Trusted-code territory: the library is mapped for the whole process lifetime (D5, ruling R1 — teardown-only unloading, never `dlclose`; a reloaded plugin leaks its old image by design).

## Invariants

- Adapter ordering is contract: `setup()` → register the `teardown` effect *first* → then register bridges and `Invoker`-backed provides. LIFO makes teardown run after every bridge/provide disposer. `dougong-wasm`'s `WasmPlugin` mirrors this; keep the two in sync.
- Raw pointers cross the boundary (`Exports.table`, `Handle`). Their `unsafe impl Send/Sync` rests on the **per-instance lock** (`InstanceLock = Arc<tokio::sync::Mutex<()>>`): every foreign entry — service calls, event dispatches, deferred continuations, teardown — holds it across the call. Consumer fibers run on their own tasks and the kernel does not order cross-fiber calls, so bypassing the lock voids the safety argument.
- Event surface is ABI v2 CPS (D14): the descriptor's `listen` map (name → u32 id) drives `Context::on_dispatch` bridges. A waterfall `STATUS_DEFERRED` is followed by `next.call` and a synchronous `on_next`, all under the same lock hold — plugin state cannot observe interleaving.
- Output buffers grow on `STATUS_CAPACITY` (doubled, bounded at 16 MiB via `Exports::growable_call`) — never convert capacity into a hard failure.
- `DynPlugin` implements the kernel `Plugin` trait; the descriptor JSON (`name`/`provide`/`inject`/`listen`) drives `inject()`, the provide list, and bridge registration. The loader never overrides plugin-declared requirements.
- `load` gates on `ABI_VERSION` from `DOUGONG_PLUGIN_EXPORTS`; a mismatch is a hard load error, not a warning.

## Tests

`cargo test -p dougong-dylib` uses static fake export tables (`from_static`) — extend them whenever `dougong-abi` changes shape. The end-to-end path is `examples/dylib_demo.rs`, which requires `libgreeter_plugin.so` in `target/debug` (built by `cargo build --workspace`).
