# CLAUDE.md — dougong (kernel)

The composition kernel. Everything else in the workspace adapts to its semantics; read the root CLAUDE.md first.

## Invariants that span files (do not break)

- **Effect collection**: every environmental mutation goes through `Fiber::effect`; disposers unwind LIFO. `Context::provide`/`on`/`accessor` are thin wrappers over it — never mutate shared state outside this mechanism in a plugin-facing path.
- **ω before body**: the load path commits the committed view (store + provider candidates) *before* running the plugin body; the acceptance tests pin this (a provider can read its own service during activation; late consumers must not stay Pending).
- **Panic containment**: the plugin body runs on its own tokio task; a panic becomes a JoinError and maps to `fail_load`. The driver task must always survive.
- **Restart force**: `force = loaded != EPOCH_INACTIVE` — restarts of an inactive fiber must not trigger a spurious third activation.
- **`get`/`require` semantics** mirror TS Algorithm 6 (see root CLAUDE.md); the two error kinds are load-bearing for the boundary hosts' error mapping.
- **Drain ordering**: provider disposal drains consumers before its own effects unwind (`service.rs` disposer `join`s consumers). The reverse-dependency index (`relied`) is O(1) and must stay consistent across commit/refresh/dispose — the linear scan remains as a debug reconciliation path only.
- **Events**: five modes at dispatch time; `internal/*` bypasses `internal/dispatch` recursion; `internal/update` may veto config updates (`Fiber::update`).
- **Parking_lot everywhere**: std Mutex poisoning is deliberately absent; do not reintroduce `std::sync::Mutex`.

## Where things live

`fiber.rs` is the core (driver loop, epochs, effects, drain); `context.rs` is the public surface; `service.rs` owns the reflect store + reverse index; `events.rs` the five modes; `registry.rs` spawns child fibers with the parent-cascade effect and the initial `check_impl` probe.

## Tests

`tests/kernel_semantics.rs` (behavior contract) and `tests/acceptance.rs` (paper §15 scenario mapping) are the regression net — any semantic change updates them, and any bug fix here should first reproduce as a failing case there. Run with `cargo test -p dougong`.
