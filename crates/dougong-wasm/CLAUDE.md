# CLAUDE.md — dougong-wasm

wasm component host (wasmtime 48). The sandbox split: fuel per export call (hard compute cap), epoch yielding (cooperative scheduling via the ticker spawned in `WasmHost::new` — requires a tokio runtime), `StoreLimits` (memory/instance caps), and a capability-empty `WasiCtx` (the wasip2 guest runtime always declares `wasi:io` imports; empty ctx keeps them inert).

## Store exclusivity model (the subtle part)

One store = one execution. All guest entries funnel through `channel.rs::GuestChannel`, which holds a `tokio::sync::Mutex<Store>` plus an execution marker:

- The marker is written **only after acquiring the store lock** (writing it earlier lets waiters overwrite the holder's task id and turns same-task reentry into a self-deadlock).
- Same-task reentry is rejected *before* the lock (`reject_self_reentry`) — otherwise the guest calling its own service would block on the mutex forever.
- Concurrent callers (e.g. a consumer notified mid-`apply`) wait; that wait is correct and must not be replaced by `try_lock`.

The `clippy::significant_drop_tightening` allowances on the store guards are intentional: the guard must span the awaited export call.

## Wasmtime 48 API facts (verified empirically)

`instantiate_async` returns the wrapper directly; component linkers are `wasmtime::component::Linker` bound via `add_to_linker::<T, HasSelf<T>>`; import trait methods return the flat WIT result (traps are impossible in their signatures — degrade failures to `kernel-error::internal`); imported resources need `HostRegistration::drop` implemented via `ResourceTable::delete`; `WasiView::ctx` returns `WasiCtxView{ctx, table}`. The bindgen output is generated into `mod bindings` in `lib.rs` — regenerate, never hand-edit. Note `gen` is a reserved keyword in edition 2024.

## Contract

`wit/dougong-plugin.wit` is the single source (`dougong:kernel@0.1.0`); the host-side mapping of kernel errors lives in `error.rs` (`to_wit`/`to_kernel` preserve the two-level resolution distinction). `CONTRACT_ABI_HASH` stamps every service/event key. Teardown handle `0` is reserved; `effect-collect` allocates from 1. `arm_budget` re-arms fuel and the epoch deadline (`delta = WasmLimits.epoch_ticks_per_yield`) before every guest entry.

## Testing

`cargo test -p dougong-wasm` covers host-side mapping only. The full path is `examples/wasm_demo.rs`, which needs the guest artifact: `cargo build -p greeter-wasm --target wasm32-wasip2` (or point `DOUGONG_GREETER_WASM` at a component).
