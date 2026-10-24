# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

dougong (斗拱) is a Rust implementation of the cordis composition kernel: time composability (every environmental mutation is a revertible effect, undone LIFO) meets spatial composability (declared dependencies resolve reactively as providers come and go). Plugins come in three forms sharing one semantic: in-process Rust (`dougong::Plugin`), dylib/so via C-ABI (trusted code), and wasm components (sandboxed).

`doc/research/` is the semantic source of truth: `cordis.md` (the paper), `study-1..4` (evidence), `dougong-design.md` (decisions D4–D15 plus user rulings R1–R4). When changing behavior that a D-decision governs, update its landing record there.

## Commands

```sh
cargo build --workspace                                # host build
cargo build -p greeter-wasm --target wasm32-wasip2     # guest component (required before wasm_demo)
cargo test --workspace                                 # full suite
cargo test -p dougong --test acceptance                # one test target
cargo test -p dougong provider_identity -- --nocapture # one test by name

# Quality gates - ALL FOUR must pass with zero output / exit 0 before committing:
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# End-to-end demos (each must exit 0):
cargo run -p dougong-dylib  --example dylib_demo    # needs: cargo build --workspace (builds greeter-plugin)
cargo run -p dougong-dylib  --example outbound_demo # ABI v3 outbound arms; same dylib artifact
cargo run -p dougong-wasm   --example wasm_demo     # needs the wasm32-wasip2 guest artifact
cargo run -p dougong-loader --example loader_demo   # needs the dylib artifact
cargo run -p dougong-loader --example harness_demo  # multi-plugin composition: needs the dylib artifact + wasip2 guest
```

The toolchain is pinned patch-level in `rust-toolchain.toml` (1.99.1) because clippy/rustdoc lints drift between releases. No MSRV is declared or claimed anywhere in the manifests - the pinned toolchain is the only verified one (ruling R4). rustup auto-installs the pinned toolchain and targets on first entry.

## Architecture

Dependency direction: `dougong-abi` (zero-dep C contract) ← `dougong` (kernel) ← `dougong-dylib` / `dougong-wasm` (boundary hosts) ← `dougong-loader` (declarative reconcile). `greeter-plugin` (cdylib) depends only on `dougong-abi`; `greeter-wasm` depends only on `wit-bindgen` + `wit/dougong-plugin.wit`.

Cross-cutting semantics that span files:

- **Fiber driver model (D4)**: each fiber owns one convergence loop (`fiber.rs::drive_loop`). Load order matters: the committed view ω is committed *before* the plugin body runs; a body panic is contained as `fail_load` (body runs on its own task, JoinError becomes failure) — never let a plugin panic kill the driver task.
- **Invariant checkpoints (S1)**: every fiber lifecycle transition (spawn/load/unload/fail/dispose/driver landing) runs the matching `invariant.rs` checkpoint — release keeps O(1) epoch/uid relations, debug adds store/index/registry cross-consistency. A breach panics with a locating snapshot (kernel bug, not a recoverable error). Wire the checkpoint when adding a transition; deep checks must stay dispose-race-tolerant (present ⇒ exact, absent ⇒ tolerated).
- **Cancel scopes (S2)**: every task a fiber owns (listener invocations, the plugin body) spawns through its per-activation `CancelScope` generation (`cancel.rs`); unload and the load rollback reap the scope *before* draining effects, so no spawned guest call outlives teardown, and `dispose` cancels up front so a hung body cannot park the driver forever. Boundary work (bridges, wasm bodies) is tracked-only — never dropped mid-call (a wasmtime store forbids abandoning an in-flight `call_async`), so `Plugin::body_abort_safe` returns false only for wasm. In-process work aborts at yield points. `parallel` treats a mid-dispatch unload as listener-absent, never a failure; serial/bail/waterfall skip dead listeners (waterfall: no veto). The emitter-side dispatch task stays untracked (C7).
- **Reactive resolution**: consumers declare `inject`; a fiber parks in `Pending` until all deps are available, then converges on the desired epoch (string of provider uids). Provider withdrawal drains consumers before the provider finishes unwinding: the provide-disposer removes the impl, notifies dependents, and joins every touched fiber (`service.rs`, `relied()` reverse index).
- **Two-level capability check (TS Alg. 6)**: `Context::require` distinguishes `inactive_dependency` (declared but not committed) from `undeclared_dependency` (never declared). Boundary hosts must preserve this distinction when mapping to WIT `kernel-error`.
- **Dynamic channel (D6)**: cross-boundary services are `service-key{name, abi-hash} + method: u32 + bytes`, exposed through `dougong::Invoker`/`Invoke`. In-process typed keys (`Any` downcast) coexist; the kernel stores plain names.
- **Boundary adapters are isomorphic**: `dougong-dylib`'s `DynPlugin` and `dougong-wasm`'s `WasmPlugin` follow the identical ordering — register the teardown effect *first* (LIFO runs it last), then let the guest/exports provide; and both serialize every foreign call per instance (dylib's `InstanceLock`, wasm's store mutex) — consumer fibers call from their own tasks, so the kernel does not order those calls for you. Keep them in sync.
- **Event modes live at dispatch**: emit/parallel/serial/bail/waterfall are chosen by the dispatcher (`events.rs`), not at registration. `internal/*` events are framework extension points with scope filtering. In waterfall, a listener pulls `Next` to advance; dropping it vetoes the chain. Mode-aware bridges (`Context::on_dispatch`) receive the dispatch mode — boundary adapters use them, and `internal/*` names reject bridges.
- **Dylib events are CPS (D14, ABI v2)**: the descriptor's `listen` map registers bridges; in waterfall the plugin returns `STATUS_DEFERRED` from `on_event` (output = continuation payload, empty = original) and the host re-enters through `on_next` with the chain result. The v1 synchronous `NextFn` was removed — do not reintroduce sync continuations across the C boundary.
- **Isolation labels (D15)**: `Context::isolate(name)` mints a *fresh* label on every call; per-name overrides via `isolate_with_label` need a caller-held mapping (`dougong-loader`'s `scopes` map). Loader `isolate` rows are **realms** (`Context::isolate_realm`, D15): member provides land in the realm label, member injections prefer realm-local providers then fall back to the global default; realm services never leak to the default label.
- **WIT is the wasm contract's single source** (`wit/dougong-plugin.wit`, package `dougong:kernel@0.1.0`). Contract changes go through `dougong-design.md` D7's landing record.
- **Sandbox split (D5/D11)**: dylib = trusted, teardown-only (never `dlclose`; R1 accepted). wasm = untrusted: fuel per call + epoch yielding + `StoreLimits` + capability-empty WASI. The world import surface is the whole authority.

## Workspace discipline

- Lints are maximal (`pedantic + nursery + restriction` plus strict rust lints). The only allowed lints are documented-in-place: `cargo_common_metadata` (unpublished workspace), `multiple_crate_versions` (wasmtime/wit-bindgen pin different `syn` majors), and `significant_drop_tightening` on the wasm store guards (the guard deliberately spans the awaited export call — wasmtime serializes execution per store).
- Generated `bindgen!` output lives in `crates/dougong-wasm/src/lib.rs` under `mod bindings` with scoped allowances; do not hand-edit it.
- No `eprintln!`/`println!` in library or example code — log through `ctx.logger()`. No placeholder/`todo!` code; no `unwrap`.
- Comment only complex code, in American English; files compliant with the Rust guideline skill carry the `// Rust guideline compliant <date>` header.
- Wasmtime 48 API notes verified empirically (do not trust older memory): `instantiate_async` returns the wrapper directly (no tuple); component links need `wasmtime::component::Linker` + `add_to_linker::<T, HasSelf<T>>`; import trait signatures are flat WIT results (no `wasmtime::Result` wrapper); imported resources require implementing `Host::drop` with `ResourceTable::delete`; `WasiView::ctx` returns `WasiCtxView`.
- Long-running work is tracked under `.planning/` as named plans (e.g. `2026-10-20-dougong-impl`, `2026-10-21-full-code-review`), each with task_plan / findings / progress; pin a session with `PLAN_ID` when several run in parallel.
