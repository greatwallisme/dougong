# Decision: dougong as a standalone crate

**Goal**: Decide whether dougong's continuation should treat it as a standalone,
out-of-tree consumable crate set, backed by audited evidence.

**Next Step**: Awaiting user adoption. If adopted, Phase 3 starts by filling
`repository` in the root `Cargo.toml` (needs the canonical remote URL).

### Phase 1: Evidence audit - **Status:** complete
Read root + all 7 member manifests; probed three cordis-ecosystem reference
repos (dshbox__cordis-rs, bayunai__cordis-rs, dsh-rs); read dougong-design.md
sections 3/4/6. Evidence in findings.md.

### Phase 2: Decision - **Status:** complete
YES - standalone crate set, one workspace, per-crate publish, unified 0.x
version. Rationale and gap list in findings.md.

### Phase 3: Publishing readiness (blocked on user adoption + canonical repo URL) - **Status:** pending
1. Fill `repository` in `[workspace.package]`.
2. Flip workspace `publish = false` to per-crate: `true` for dougong-abi /
   dougong / dougong-dylib / dougong-wasm / dougong-loader; `false` stays for
   greeter-plugin / greeter-wasm (examples; greeter-plugin is a path
   dev-dep of dougong-dylib, which is publish-legal).
3. Add `version` next to `path` on all intra-workspace deps
   (`dougong = { path = "../dougong", version = "0.1.0" }` pattern) - the
   cargo requirement for publishing workspace members that inter-depend.
4. Remove the `cargo_common_metadata = "allow"` (and its comment) once
   repository lands; the lint must pass for real.
5. All four quality gates green; `cargo package --list` per publishable crate
   (offline) to confirm no stray files ship.
6. MSRV: no compatibility floor is declared anywhere (removed 2026-10-21 by
   user ruling - the 1.85 claim was never compiled against). If a floor is
   ever needed, prove it in isolated CI first, then declare it.

### Phase 4: Record + docs (blocked on Phase 3) - **Status:** pending
1. Record the decision as **D14** in `doc/research/dougong-design.md`
   (next number after D13; landing record per repo rule).
2. README: external-consumption quickstart (`dougong = "0.1"`) alongside the
   in-workspace one.
3. dougong-plugin-dev skill: add registry-dependency alternative next to the
   path-dep instructions in references/dylib-plugin.md and wasm-plugin.md
   (WIT travels by repo tag until a WIT registry is chosen).

## Decisions Made
| Decision | Rationale |
|----------|-----------|
| Standalone = crate SET, one workspace, unified version | Keeps the three plugin forms mutually compatible; mirrors dshbox__cordis-rs (single workspace version 0.6.1 across crates) |
| NOT one merged mega-crate | wasmtime (heavy) must stay out of in-process consumers; the crate split IS the dependency hygiene; confirmed dougong kernel deps are parking_lot/tokio/serde/serde_json/thiserror only |
| NOT a repo split | Design doc section 4: one whole codebase, no phasing; publishing is metadata on top, not restructuring |
| 0.x unified versioning | Low ceremony while APIs churn; fine-grained anchors already exist (ABI_VERSION=1, WIT world @0.1.0, channel hash) |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| (none) | - | - |
