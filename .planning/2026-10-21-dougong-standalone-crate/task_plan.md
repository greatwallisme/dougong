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

### Phase 3: Publishing readiness + publication - **Status:** complete (2026-10-21)
User supplied the canonical remote (github.com/greatwallisme/dougong) and the
crates.io token. Executed:
1. `repository` filled in [workspace.package]; per-member
   repository/readme/keywords/categories inheritance lines added.
2. Workspace publish flag lifted; greeter-plugin / greeter-wasm stay
   `publish = false` (examples).
3. Intra-workspace path deps carry `version = "0.1.0"`.
4. `cargo_common_metadata` allow removed - the lint passes for real.
5. Fix discovered during publishing: workspace-inherited `readme` resolves
   verbatim per member (../../README.md nonexistent) -> field removed;
   cargo_common_metadata then required a readme on publishable crates ->
   five short per-crate READMEs written (real content, not stubs).
   Second fix: the packaged dougong-wasm tarball lacked ../../wit ->
   the WIT single source (D7) physically relocated into
   crates/dougong-wasm/wit/, root wit/ is a symlink to it.
6. ALL FIVE CRATES PUBLISHED to crates.io at v0.1.0 (dougong-abi, dougong,
   dougong-dylib, dougong-wasm, dougong-loader) via
   `cargo publish --registry crates-io` in dependency order (the local
   cargo replaces crates-io with the read-only rsproxy mirror, so the
   --registry flag is mandatory here).
7. Pushed to github.com/greatwallisme/dougong (master), tag v0.1.0.

### Phase 3b: Gates during publication - **Status:** complete
clippy/test/fmt/doc exit 0 with zero warnings at every step; the two
packaging fixes (readme, WIT relocation) were gate-verified before retry.

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
