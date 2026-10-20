# Findings: dougong standalone-crate decision (2026-10-21)

## Evidence

### E1 - dougong's own manifests are publish-ready except two known gaps
- Root `Cargo.toml` `[workspace.package]`: version 0.1.0, edition 2024,
  license MIT OR Apache-2.0, readme, keywords, categories - all populated. `publish = false` with the comment: "Local workspace: not yet
  published. Fill `repository` and flip this to `true` per crate when
  publishing."
- The `cargo_common_metadata = "allow"` lint comment names the same single
  blocker: "`repository` is pending until the project gets its canonical
  remote; all other metadata fields are populated. Revisit before publishing."
- MSRV: none. The `rust-version = "1.85"` declaration was removed everywhere
  on 2026-10-21 by user ruling - it was never compiled against, so claiming
  it was unevidenced. Only the pinned 1.99.1 toolchain is verified. R2
  (musl) already provisioned: `x86_64-unknown-linux-musl` target in the
  toolchain file.

### E2 - dependency layering already gives per-consumer minimality
- `dougong` (kernel): parking_lot, tokio, serde, serde_json, thiserror. No
  wasmtime, no libloading. An in-process-plugin consumer takes `dougong`
  alone.
- `dougong-abi`: zero dependencies (by design, dougong-abi/CLAUDE.md).
- `dougong-dylib`: dougong + dougong-abi + libloading (+ greeter-plugin as
  path DEV-dependency - dev-deps may stay path-only for publishing).
- `dougong-wasm`: dougong + wasmtime/wasmtime-wasi 48 - opt-in weight.
- `dougong-loader`: dougong + both boundary hosts (+ serde for fingerprints).
  Loader users transitively get wasmtime; acceptable, document it.

### E3 - ecosystem precedent: cordis kernels ship as standalone versioned crates
- `dsh-rs` (real downstream harness) depends on the kernel via registry:
  `cordis = { version = "0.2", package = "cordis-rust" }` with the in-file
  comment "The kernel (published on crates.io as cordis-rust)". Direct proof
  the reference ecosystem consumes kernels as external versioned crates.
- `dshbox__cordis-rs`: multi-crate workspace, ONE workspace version (0.6.1)
  across all members, `repository` filled, and workspace dependency entries
  carry `path + version` dual specs - the canonical publishable form.
- `bayunai__cordis-rs`: same shape (repository/homepage/documentation filled).

### E4 - dougong's contracts were designed for out-of-tree consumers
- ABI_VERSION = 1 gate at load (dougong-dylib/CLAUDE.md: "a mismatch is a hard
  load error").
- WIT world `dougong:kernel@0.1.0` is "the wasm contract's single source"
  (root CLAUDE.md); D7 governs its changes.
- Channel hash `dougong-dynamic-channel-v1` stamps cross-boundary keys.
- R3 (adjudicated 2026-10-20) picked the contract crate NAME `dougong-abi` -
  a naming decision that only matters for external consumers.

### E5 - the design doc's delivery philosophy is compatible
- Section 4: "one whole codebase, no phasing" - publishing readiness is
  manifest metadata + docs, not a restructuring or a phased split. The
  remaining R-items were adjudicated; distribution was never ruled against,
  it simply predates a canonical remote.

### E6 - cross-toolchain plugin loading, proven (2026-10-21)
A/B experiment on the real repo: built `libgreeter_plugin.so` with rustc
1.96.0 (rust-toolchain.toml pins 1.99.1 for the host), loaded it with the
1.99.1-built `dylib_demo`. Baseline (1.99.1/1.99.1) exit 0; treatment
(1.96.0 .so + 1.99.1 host) exit 0 with the full round trip: descriptor load,
dynamic-channel call ("hello from a dougong dylib"), teardown. Toolchain
versions are orthogonal to plugin compatibility on the dylib boundary; the
load gate is `ABI_VERSION` (`crates/dougong-dylib/src/lib.rs:267` hard-errors
on mismatch; negative test builds `ABI_VERSION + 1` at :385). Mechanism:
every type in `dougong-abi` is `#[repr(C)]` / `extern "C"` with primitive
types and caller-owned buffers only (`crates/dougong-abi/src/lib.rs:15,208`).
External plugin authors therefore couple to the CONTRACT version, not to the
host's compiler - which is what makes the standalone-crate-set packaging
viable (E1-E5).

## Gap list (all of Phase 3 in task_plan.md)
1. `repository` URL - the only substantive unknown; needs the canonical
   remote (user input).
2. `publish` flags + `path + version` dual specs - mechanical.
3. `cargo_common_metadata` allow removal - one line, after (1).
