# CLAUDE.md — dougong-loader

Declarative desired-state JSON + diff reconcile (design decision D9). The kernel owns the intent surface; this crate only diffs documents and drives `ctx.plugin` / `FiberHandle::update` / `ctx.uninstall`.

## Semantics

- The document is the **full desired state**, not a patch. `apply` is idempotent: the document stays installed until `replace` swaps it, and a second `apply` over an unchanged document must be `converged()`.
- Reconcile rules: absent → insert; config fingerprint changed → `handle.update` (kernel restart-style hot update via `internal/update`); dropped → uninstall; isolate changed → retire + re-insert (scope is baked into the fiber's context at start and cannot migrate).
- **Scope→label mapping lives here** (`scopes: HashMap<String, u64>`): `Context::isolate(name)` mints a fresh label per call, so plugins sharing a document realm would silently land in different labels without this map. Always allocate via `ctx.fresh_label()` and reuse it for the same scope name.
- **Rows with `isolate` are realms (D15)**: each member starts in `ctx.isolate_realm(label)` - its provides land in the realm label and its injections prefer realm-local providers, falling back to the global default for names the realm lacks. Same-name seams can exist once per realm without colliding.
- Per-slot failures are collected in `Report::failures`; one broken artifact must never block the rest of the document.
- Fingerprint = `serde_json` `to_string`; maps are ordered, so it is canonical (key-order insensitive — there is a unit test).
- `builtin` sources resolve from the registry passed to `Reconciler::new`: the harness's own in-process plugins share the declarative path with external artifacts.

## Deliberately out of scope

File watching. A watcher is an event source, not reconciliation logic — feed the new document to `replace` + `apply` from any trigger.

## Tests

`cargo test -p dougong-loader` (behavior: lifecycle, scope sharing, failure isolation; plus fingerprint). `examples/loader_demo.rs` runs the full lifecycle against the dylib greeter artifact (built by `cargo build --workspace`). `examples/harness_demo.rs` walks the full developer workflow against two real document files (`harness.json`, `harness.upper.json` in the same directory): ten slots from one file - in-process plugins with cross-plugin injects split across TWO isolate realms (per-session capability sets: same `harness.llm` seam, echo in session-a, upper in session-b), greeter dylib + wasm mixed in - plus a Pending phase (agents park without providers), a config-only seam swap, and a retire phase that deletes the `agent-b` row and uninstalls exactly that slot.
