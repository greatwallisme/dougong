# CLAUDE.md — dougong (kernel)

The composition kernel. Everything else in the workspace adapts to its semantics; read the root CLAUDE.md first.

## Invariants that span files (do not break)

- **Effect collection**: every environmental mutation goes through `Fiber::effect`; disposers unwind LIFO. `Context::provide`/`on`/`accessor` are thin wrappers over it — never mutate shared state outside this mechanism in a plugin-facing path.
- **ω before body**: the load path commits the committed view (store + provider candidates) *before* running the plugin body; the acceptance tests pin this (a provider can read its own service during activation; late consumers must not stay Pending).
- **Panic containment**: the plugin body runs on its own tokio task; a panic becomes a JoinError and maps to `fail_load`. The driver task must always survive.
- **Restart force**: `force = loaded != EPOCH_INACTIVE` — restarts of an inactive fiber must not trigger a spurious third activation.
- **`get`/`require` semantics** mirror TS Algorithm 6 (see root CLAUDE.md); the two error kinds are load-bearing for the boundary hosts' error mapping.
- **Drain ordering**: the provide-disposer removes the impl, notifies dependents, and joins every touched fiber, so consumers converge before the provider finishes unwinding (`service.rs`). The reverse-dependency index (`relied`) is O(1) and must stay consistent across commit/uncommit/dispose.
- **Invariant checkpoints (S1)**: every fiber lifecycle transition (spawn/load/unload/fail/dispose/driver landing) runs the matching `invariant.rs` checkpoint; a breach panics with a locating snapshot (a kernel bug, never a recoverable error). Release builds keep the O(1) field-relation tier; `debug_assertions` adds store/index/registry cross-consistency. Deep checks collect violations under the lock and breach after dropping it, and tolerate dispose-raced absence (present ⇒ must match exactly, absent ⇒ tolerated). New transitions must wire their checkpoint.
- **Cancel scopes (S2)**: every task a fiber owns spawns through `Fiber::task_scope` (`cancel.rs`), whose gate rejects spawns into a cancelled generation. `unload_once` and the load-rollback arms reap the scope *before* draining effects; `dispose` cancels up front (never reaps — it may be running on one of the fiber's own listener tasks). Bridges and wasm bodies are tracked-only; in-process futures abort at yield points. The invariant layer asserts `cancelled ⇒ task_count == 0` at post_unload/post_fail_load/post_dispose. The driver task itself and the emit dispatcher stay outside the scope (the driver is the reaper; C7 keeps emitter dispatch alive).
- **Timeout budgets (S3)**: `RootState::timeouts` (`TimeoutConfig`, construction-fixed, default all-`None`) bounds five waits — `apply` (body; rollback then Fail, or bounded Retry decided by the driver — never retry/park a stale target), `listener` (per invocation; plain = in-task drop, bridge = tracked spawn + await-side detach; `internal/config` resolves under it as a whole chain), `drain` (per-consumer join in the provide/accessor disposers; CancelConsumer cancels the stuck consumer's scope), `cleanup` (orphanable disposers run on detached tasks, timed-out ones keep running — never aborted; Critical kernel disposers are exempt), and `reap_watchdog` (logs `reap_blocked` per interval, never proceeds). Because the drain budget weakens the Thm 63 wait, the debug-tier residue checks exempt the `provider_consumers[self]` arm exactly when a drain budget is configured; a stuck consumer's stale edges self-clean on its later convergence or disposal.
- **Events**: five modes at dispatch time, threaded to `DispatchListener` bridges (`Context::on_dispatch`; `internal/*` names reject bridges); `internal/*` bypasses `internal/dispatch` recursion; `internal/update` may veto config updates (`Fiber::update`) — the accepted config is persisted only in the waterfall tail, so a veto leaves the previous config live.
- **Parking_lot everywhere**: std Mutex poisoning is deliberately absent; do not reintroduce `std::sync::Mutex`.

## Where things live

`fiber.rs` is the core (driver loop, epochs, effects, drain); `context.rs` is the public surface; `service.rs` owns the reflect store + reverse index; `events.rs` the five modes; `registry.rs` spawns child fibers with the parent-cascade effect and the initial `check_impl` probe; `invariant.rs` hosts the transition checkpoints (fault-injection unit tests live in its `#[cfg(test)]` module).

## Tests

`tests/kernel_semantics.rs` (behavior contract) and `tests/acceptance.rs` (paper §15 scenario mapping) are the regression net — any semantic change updates them, and any bug fix here should first reproduce as a failing case there. Run with `cargo test -p dougong`.
