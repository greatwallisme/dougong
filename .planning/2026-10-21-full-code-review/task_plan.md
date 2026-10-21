# Full code review (project start to HEAD) with cordis-fidelity check

**Goal**: Review every line of shipped code (~5.8k src + 0.8k tests) for
correctness bugs, reuse/efficiency issues, and fidelity to the original
cordis semantics (doc/research/cordis.md + dougong-design.md D1-D13). Apply
fixes inline (--fix), all four gates must stay green, findings uncapped.

**Next Step**: Kernel core read (fiber.rs), notes to findings.md.

### Phase 1: Inventory + reference anchors - **Status:** complete
15 commits, 24 src files / 5,822 LOC, 3 test files / 833 LOC. Review order:
kernel core -> abi -> dylib -> wasm -> loader -> greeters -> fidelity pass.

### Phase 2: Kernel review (dougong) - **Status:** complete
fiber.rs (1055), service.rs (463), context.rs (596), events.rs (401),
registry.rs (193), plugin.rs (130), scope.rs (133), invoke.rs (67),
error.rs (108), logger.rs (81), lib.rs (72).

### Phase 3: Boundary review (abi, dylib, wasm) - **Status:** complete

### Phase 4: Loader + greeters - **Status:** complete

### Phase 5: cordis fidelity pass - **Status:** complete
Anchors confirmed: cordis.md:1696 (Alg 6 context access -> two-level
require mapping preserved across the wasm boundary, error.rs:16-43),
cordis.md:2154 (Thm 63: dependents drain before provider inverse ->
provide-disposer joins touched consumers, service.rs:297-314),
cordis.md:1685 (notify/drains dependents), five event modes match
is_bailed/serial/bail/waterfall semantics. ONE fidelity GAP found, not
fixed by decision (see findings.md F6): the dylib on_event export is never
dispatched by the host and the descriptor has no listen declaration - the
wasm boundary has full event support, dylib does not. Wiring it requires a
new descriptor contract element plus a sync-ABI async-continuation design
(D8-governed); semantics separable from the kernel core, deferred with
justification for a user ruling.

### Phase 6: Fix application + gates + commit - **Status:** in_progress
Applied: F1 (update veto config leak + regression test), F3 (dead probe in
notify), F4 (active-set cleanup on dispose), F5 (committed_providers dead
field removal), F7 (STATUS_CAPACITY grow-retry + test), F10
(epoch_ticks_per_yield dead knob now honored, widened to u64 per wasmtime
48 API). Gates running.

## Decisions Made
| Decision | Rationale |
|----------|-----------|
| Review runs inline (0 agents) | User cap is "at most 3 agents"; the subagent backend is deterministically broken (400, nonexistent model ID), so inline is the only working route |
| Findings uncapped but evidence-only | User set no issue cap; every reported finding must cite file:line and be reproducible or demonstrable |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| /code-review fork died at startup | 1 | API 400, nonexistent model ID (same fault as skill-creator evals); executed inline instead |
