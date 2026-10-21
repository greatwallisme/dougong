# Multi-plugin composition example (deepseek-harness parity)

**Goal**: The current examples prove single mechanisms; real usage is many
plugins with inter-plugin dependencies. Add a composite example that mirrors
the deepseek-harness (dsh) architecture: every part is a plugin, capability
seams are swapped by config, isolate realms give sessions different capability
sets, and events are the extension points.

**Next Step**: Phase 2 - implement `crates/harness-demo`.

### Phase 1: Study dsh architecture - **Status:** complete
Read `deepseek-harness/docs/architecture.md` (TS monorepo, cordis-based).
Extracted patterns -> findings.md.

### Phase 2: Design + implement harness example - **Status:** complete
USER RULING: not a new crate - an `examples/` entry. Home:
`crates/dougong-loader/examples/harness_demo.rs`. Implemented and run green:
9 slots (settings/telemetry/tools/guard/llm/agent-a/agent-b + greeter dylib +
wasm), cross-plugin injects, guard veto via tools/pre-execute waterfall,
config-only seam swap (echo -> upper) hot-restarting the row and reactively
rewiring consumers. Design adaptation discovered during implementation: the
loader reconcile keys on CONFIG fingerprint (source is slot-invariant), so
the seam swap selects the adapter through row config (`{"adapter": ...}`) -
which matches dsh semantics (a patch replaces the row's config).

### Phase 2b: README provenance + alignment - **Status:** complete
README gained "参考与致谢" (project credit table) and "与原版 cordis 的对齐
程度" (aligned / dougong-specific / intentionally-not-aligned); cordis.md
carries a provenance note (paper = arXiv:2608.25512 PKU/DeepSeek-AI,
cordiverse/paper + cordiverse/cordis; the analysis file is third-party,
obtained via a third-party repository, archived as reference only). The exact
third-party repo name is not recorded anywhere in study files - left unnamed
rather than guessed; user can supply it to complete the citation.

### Phase 3: Gates + all demos green - **Status:** complete
fmt/clippy/test(16 suites)/doc all exit 0, zero warnings; all four demos
exit 0 (dylib/wasm/loader/harness).

### Phase 4: Docs + commit - **Status:** complete
root CLAUDE.md demos + loader CLAUDE.md updated; committed.

## Decisions Made
| Decision | Rationale |
|----------|-----------|
| One composite crate, not more per-crate demos | The module demos stay as single-mechanism intros; composition needs one runnable story with assertions |
| Per-realm difference via the llm seam (same service name, different labels), not via event filtering | Isolation labels are dougong's real "different capability set per session" mechanism (dsh: "a service row there needs an isolate realm"); plain event listeners are unfiltered by design |
| Provider swap via `Reconciler::replace` + `apply` | dsh's profiles/patches = dougong's desired-state documents; the swap proves reactive rewiring (consumer restarts on provider-identity change) |
| Failure isolation shown with the wasm row | Per-slot failures must not block the document (D9); documented prerequisite for the wasip2 artifact |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| (none) | - | - |
