[English](README.md) | [简体中文](README.zh-CN.md)

# dougong

A Rust implementation of cordis: a general-purpose plugin foundation kernel with temporal × spatial composability.

- **Temporal composability**: every environmental mutation is a revertible effect (collected LIFO; unloading rolls back in reverse registration order; teardown is derived from setup)
- **Spatial composability**: dependencies are declared explicitly and resolved reactively by availability — when a provider arrives late, leaves, or gets replaced, consumers rewire automatically
- **Plugin forms**: `dylib/so` dynamic libraries (C-ABI, trusted code) and `wasm` components (sandboxed, capability-minimized)
- **Research docs**: [doc/research/](doc/research/) ([cordis.md](doc/research/cordis.md) full-paper analysis, TS-version comparison, ecosystem survey, [design decisions & rulings](doc/research/dougong-design.md))

## Crates

| crate | contents |
|-------|----------|
| `dougong` | The composition kernel: context / fiber / dependency injection / revertible effects / reactive notification / the five event modes / reverse dependency index |
| `dougong-abi` | The C-ABI contract for dylib plugins (zero dependencies) |
| `dougong-dylib` | The dylib/so host: libloading + the ABI gate + the dynamic channel adapter |
| `dougong-wasm` | The wasm component host: wasmtime + fuel/epoch/memory limits + a minimal WASI |
| `dougong-loader` | Declarative JSON expected state + diff reconcile (insert/update/retire/isolate) |
| `greeter-plugin` | Example dylib plugin (pure C-ABI cdylib) |
| `greeter-wasm` | Example wasm plugin (wit-bindgen, `wasm32-wasip2`) |

## Quick start

Host projects depend on the crates directly from crates.io:

```toml
[dependencies]
dougong = "0.4"          # kernel (in-process plugin form)
# Add the boundary crate for each plugin form you use:
# dougong-dylib = "0.4"  # dylib plugin host
# dougong-wasm = "0.4"   # wasm plugin host
# dougong-loader = "0.4" # declarative config + reconcile
```

Requirements: Rust (edition 2024); development and verification use the toolchain pinned by `rust-toolchain.toml` — no lower compatibility bound is claimed; the wasm examples additionally need `rustup target add wasm32-wasip2`.

```sh
# Full host-side build + tests
cargo build --workspace
cargo test --workspace

# Build the wasm guest example component (greeter-wasm)
cargo build -p greeter-wasm --target wasm32-wasip2

# Three mechanism demos (one per boundary kind)
cargo run -p dougong-dylib  --example dylib_demo    # dylib boundary: reactive wiring + calls + drain
cargo run -p dougong-wasm   --example wasm_demo     # wasm boundary: calls + events + waterfall + drain
cargo run -p dougong-loader --example loader_demo   # declarative config: insert/update/retire

# Multi-plugin composition demo (mirrors deepseek-harness's "everything is a plugin" shape)
cargo run -p dougong-loader --example harness_demo  # 9 slots of mixed origin (builtin+dylib+wasm) in one document,
                                                    # cross-plugin dependency injection, capability-seam config swapping,
                                                    # policy guards vetoing via waterfall, failure isolation
```

## Developing dougong plugins with AI in host projects

dougong is consumed by host projects as crates (`dougong` / `dougong-dylib` / `dougong-wasm` / `dougong-loader`). This repository ships a ready-made skill for AI coding in **downstream projects**: [`skills/dougong-plugin-dev/`](skills/dougong-plugin-dev/). Copy that directory into the host project's `.claude/skills/` (tools like Claude Code discover project-level skills automatically) and the AI develops plugins against dougong's actual contracts — form selection, the dylib export table and descriptor, the v2 CPS event protocol, wasm sandbox constraints, verification commands, and common pitfalls — instead of guessing APIs from generic knowledge.

## The plugin development workflow

Four actions span a plugin's life (runnable counterpart: [`crates/dougong-loader/examples/harness_demo.rs`](crates/dougong-loader/examples/harness_demo.rs) plus [`harness.json`](crates/dougong-loader/examples/harness.json) in the same directory):

**1. Develop** — pick a form and write only "declarations + registrations"; no unload logic:

- In-process (simplest): `dougong::plugin("name", dependency list, |ctx, config| ...)`, registering capabilities with `ctx.provide("service name", value)` inside the activation body;
- dylib: a cdylib crate depending only on `dougong-abi`, writing the export table (see `greeter-plugin`);
- wasm: implement the Guest trait with wit-bindgen (see `greeter-wasm`); it runs inside a sandbox.

**2. Load** — write a JSON expected-state document (one line per plugin: name / source `builtin`·`dylib`·`wasm` / config / isolate realm — see [`harness.json`](crates/dougong-loader/examples/harness.json)); the host loads and converges:

```rust
let mut reconciler = Reconciler::new(builtin plugin table, wasm host);
reconciler.replace(Config::load_file("harness.json")?);
reconciler.apply(&ctx).await;      // every plugin in the document starts automatically
```

**3. Declare dependencies** — the plugin lists the service names it needs in `inject`; the kernel handles the rest: with no provider online the fiber parks in `Pending`, and activates automatically once one comes up (document row order is irrelevant); using an undeclared service is rejected outright (capability check); when a provider is replaced, consumers rewire automatically.

**4. Unload** — **delete that row** from the document, then `replace` + `apply` again: the plugin is unloaded precisely, its consumers drain first, and registered resources roll back automatically in LIFO order — no hand-written unload code anywhere. The code-path equivalents: `handle.dispose()` / `ctx.uninstall(&plugin)` / `ctx.stop()`.

One caveat: a dylib's code image never unloads in-process (switching .so versions = restarting the process); its config can still be hot-updated at any time via `handle.update(new config)`, unaffected.

## The kernel in one sentence

Plugins declare dependencies (`inject`) and mutate the environment only through `ctx.effect`/`ctx.provide` inside their activation body (`apply`);
while dependencies are unready the fiber parks in `Pending`, and any change in the provider set converges by epoch (migration/re-activation);
when a provider unloads, its consumers drain before its own effects roll back — LIFO guarantees the order.

## Trust boundaries and sandboxing

| Form | Trust model | Boundary |
|------|-------------|----------|
| In-process Rust | Fully trusted | None |
| `dylib/so` | Trusted code ([design decision D5](doc/research/dougong-design.md): teardown only, the image never unloads) | C-ABI contract + the ABI version gate |
| `wasm` component | Untrusted | The Component Model world is the entire authority surface; zero-capability WASI; a hard fuel budget + cooperative epoch yielding + `StoreLimits` memory/instance caps |

Cross-boundary services all travel the dynamic channel ([design decision D6](doc/research/dougong-design.md)): `service-key{name, abi-hash} + method(u32) + bytes`;
consumers resolve by declaration and undeclared use is rejected (the two-level resolution error, mirroring TS Algorithm 6).

## Quality gates

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # pedantic + nursery + restriction
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

## Version anchors

- Kernel/host: `wasmtime` 48.x (verified on 48.0.7), `wit-bindgen` 0.61.x (verified on 0.61.1), tokio, edition 2024
- WIT contract: `wit/dougong-plugin.wit` (package `dougong:kernel@0.1.0`) — the single source of truth for the wasm boundary

## References and acknowledgments

dougong stands on the shoulders of these open-source projects (the full evidence chain lives in [doc/research/study-1..4](doc/research/)):

| Project | What was drawn from it |
|---------|------------------------|
| [cordiverse/cordis](https://github.com/cordiverse/cordis) (the TS original) and [cordiverse/paper](https://github.com/cordiverse/paper) (arXiv:2608.25512, PKU / DeepSeek-AI) | The semantic source: revertible effects, reactive coeffects, epoch convergence, the five event modes; dougong is a Rust reproduction of its semantics (see the alignment section below) |
| [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) (dsh) | The application-architecture reference for "everything is a plugin": capability seams, isolate realms giving sessions different capability sets, events as extension points — see [`harness_demo`](crates/dougong-loader/examples/harness_demo.rs) |
| [redoop/cordis-rust](https://github.com/redoop/cordis-rust) | The inverted-API precedent (plugins only compute; the host owns all framework semantics) |
| [jincdream/cordis-rs](https://github.com/jincdream/cordis-rs) | The semantic alignment checklist (study-2), the baseline for `kernel_semantics.rs` |
| [cordis_wasm](https://github.com/cordiverse/cordis), [nucleus](https://github.com/meritozh/nucleus) | The Component Model route ([design decision D13](doc/research/dougong-design.md) evidence) and the WASI 0.3 comparison |
| dshbox/bayunai/helloworldtang/collab-ai-dev and other cordis-rs variants, tiny-cordis, spatiotemporal, cordis-verus, metis | Ecosystem survey and design-tradeoff evidence (study-3/4) |

## Alignment with the original cordis

- **Semantic alignment (reproduced and pinned by tests)**: revertible effects with LIFO rollback; epoch-driven convergence/re-activation; two-level dependency resolution checks (TS Algorithm 6, `inactive_dependency` vs `undeclared_dependency`); the five event modes (`isBailed` non-null bails, skipping `next` in waterfall vetoes, `internal/*` recursion protection); isolate scopes and scope shadowing; per-session capability sets via isolate realms (D15: a realm-local instance of the same service + global fallback); provider unload draining consumers first; declarative expected-state reconcile (insert/update/retire/failure isolation); plugin body panics contained as load failures. Acceptance mapping: `tests/acceptance.rs` (paper §15 scenarios), `tests/kernel_semantics.rs` (the upstream compatibility surface).
- **dougong-specific technical trade-offs (the original never faces these)**: the original is a TS architecture where the language runtime loads plugins into the same process — boundaries are naturally stable and no ABI is needed. Two of dougong's three forms cross independently compiled artifacts, and Rust has no stable ABI across artifacts, so "three forms sharing one semantic contract" can only be achieved by **explicitly defining the boundary contracts** — a trade-off, not a bonus: on the dylib side it costs the C-ABI v4 contract (bidirectional: host-driven + plugin outbound require/emit plus activation-scoped grants) plus buffer protocols and explicit serialization (event chaining also needs [design decision D14](doc/research/dougong-design.md)'s CPS scheme to route around "synchronous C callbacks cannot await"); on the wasm side it costs the sandbox engineering (capability-empty WASI + fuel/epoch/memory caps); both boundaries introduce per-instance serialization constraints. The in-process form, like the original, pays none of this.
- **Deliberately not aligned**: Node-specific mechanisms (prototype context, the pnpm/bundle/HMR ecosystem) are not ported; the capability policy library belongs to the application layer (in the dsh context) and stays out of the kernel; the ordered layering of `cordis.patch.yml` is replaced by "whole-document replace + reactive convergence" — the outcome depends only on the final document, and ambiguity is hard-rejected rather than arbitrated by layer order (dougong needs no layer order; the full argument is [design decision D9](doc/research/dougong-design.md), "the real split between declarative config and the loader").
- **Provenance of the document assets**: [cordis.md](doc/research/cordis.md) is a third-party-written full-paper analysis (the paper itself is arXiv:2608.25512, PKU/DeepSeek-AI), obtained from [acryldev/cordis-meta-framework-spatiotemporal-composability](https://github.com/acryldev/cordis-meta-framework-spatiotemporal-composability), archived here as a research reference and not a work of this project — see the provenance note inside that file.
