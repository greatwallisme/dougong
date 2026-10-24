# CLAUDE.md — greeter-wasm

Example dougong plugin as a wasm component. Depends **only** on `wit-bindgen` and `wit/dougong-plugin.wit` — no kernel, no tokio. It is the reference for writing wasm plugins and the live artifact for `dougong-wasm`'s demo.

## Build and layout

- Build with `cargo build -p greeter-wasm --target wasm32-wasip2` (plain cargo + the `wit_bindgen::generate!` macro — no cargo-component). Artifact: `target/wasm32-wasip2/debug/greeter_wasm.wasm`.
- All code is behind `#[cfg(target_arch = "wasm32")]`; host-target workspace builds compile this crate to an empty library on purpose. Do not add host-target dependencies.
- Generated bindings live in `mod bindings` (`lib.rs`) — regenerate via the macro, never hand-edit. Note: within `src/imp.rs`, imports resolve as `crate::bindings::...`.

## Contract details that bite

- Event replies are **JSON-encoded kernel values**: a bare string must be quoted (`"\"pong\""`), or the host-side decode fails.
- `apply` is where all registration happens (`provide_service`, `register_listener`, `effect_collect`); the host reverses them LIFO on withdrawal, with handle `0` = plugin teardown.
- Consumed services must appear in the descriptor's `inject` or the host rejects `call-service` (capability check).
- Guest state is a `thread_local` `RefCell` — wasm execution is single-threaded and the host serializes export calls.
- Fault-injection hooks (S7, dormant unless triggered): service-call payload `boom` traps; `ping` payload `"boom"` (JSON string) traps, `"fail"` returns a guest error; config `{"trap_teardown":true}` makes `run_disposer(0)` trap. They power `dougong-wasm/tests/fault_injection.rs`; the demo paths never send those payloads.
