# CLAUDE.md — dougong-dylib

dylib/so plugin host. Trusted-code territory: the library is mapped for the whole process lifetime (D5, ruling R1 — teardown-only unloading, never `dlclose`; a reloaded plugin leaks its old image by design).

## Invariants

- Adapter ordering is contract: `setup()` → register the `teardown` effect *first* → then register `Invoker`-backed provides. LIFO makes teardown run after every provide disposer. `dougong-wasm`'s `WasmPlugin` mirrors this; keep the two in sync.
- Raw pointers cross the boundary (`Exports.table`, `Handle`). Their `unsafe impl Send/Sync` rests on the kernel's per-fiber serialization — if you ever call exports outside the fiber driver, the safety argument is void.
- `DynPlugin` implements the kernel `Plugin` trait; the descriptor JSON (`name`/`provide`/`inject`) drives `inject()` and the provide list. The loader never overrides plugin-declared requirements.
- `load` gates on `ABI_VERSION` from `DOUGONG_PLUGIN_EXPORTS`; a mismatch is a hard load error, not a warning.

## Tests

`cargo test -p dougong-dylib` uses static fake export tables (`from_static`) — extend them whenever `dougong-abi` changes shape. The end-to-end path is `examples/dylib_demo.rs`, which requires `libgreeter_plugin.so` in `target/debug` (built by `cargo build --workspace`).
