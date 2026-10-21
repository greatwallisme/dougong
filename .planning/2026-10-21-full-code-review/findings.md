# Review findings (running notes)

## F6 - DEFERRED with justification (dylib event dispatch gap)

The `on_event` export exists in the C-ABI table (D8 put all five modes into
the contract), but `dougong-dylib` never dispatches it: `DynPlugin::apply`
only wires `setup`/`provide`, and the descriptor JSON has no `listen`
declaration, so the host cannot know what to register. The wasm boundary has
the full event surface (register_listener/handle_event/waterfall_next).

Why not fixed in this pass: wiring it is a contract-design change, not a
bug fix - it needs (a) a descriptor `listen` element (name -> u32 event id
mapping, descriptor is contract data), and (b) a design for the waterfall
continuation across a SYNC C callback (the kernel `Next` is async; a
sync `NextFn` trampoline cannot faithfully drive it without new re-entrancy
infrastructure - emit/parallel/serial/bail are straightforward, waterfall
is the hard part). That is D8-governed contract surface; inventing it
unilaterally inside a review would bypass the landing-record rule.
Recommendation for a follow-up ruling: accept "listen declaration + sync
modes now, waterfall veto-only (next always None, documented)" OR extend
the ABI async-capable (bump ABI_VERSION). Semantics are separable from the
kernel core, which is complete.

## Confirmed fixes applied (Phase 6)

### F1 (bug, fiber.rs:877): update() veto leaks the rejected config
`Fiber::update` overwrites `raw_config` BEFORE the `internal/update`
waterfall; the tail (restart) is skipped when a hook vetoes (never pulls
`next`, documented at fiber.rs:869-871 and confirmed in
events.rs:297-337 - the tail only runs via the `next` chain). The rejected
config then loads on any later restart. Fix: assign `raw_config` inside the
waterfall tail, just before `restart()`.

### F3 (dead code + latent panic shape, service.rs:418-419)
`notify` computes `let label = ctx.effective_label(&fiber.inject_names()[0]);`
then discards it (`let _ = label;`). Leftover probe: dead, and `[0]` is only
safe because key_interest membership implies a non-empty inject list.
Fix: delete both lines.

### F4 (bug, context.rs:89-92): disposed uid lingers in the root `active` set
`RootState::remove_fiber` clears the fiber table and dependency indexes but
not `active`. `Fiber::dispose` clears uid to None first, so `cache_state`
skips `mark_inactive` (fiber.rs:353 `if let Some(uid)`). Consequences:
`fiber_is_active(stale_uid)` stays true forever, and during the
dispose-to-drain window a strict resolve can still bind the dying provider.
Fix: remove the uid from `active` in `remove_fiber`.

### F5 (dead state, fiber.rs): `committed_providers` has no reader
Set at :689, cleared at :632/:725/:747, never read anywhere (rg over crates/).
The `.lock()` method call masks rustc's never-read-field lint. The
DependencyIndex already mirrors the same edges (`consumer_providers`).
Fix: delete the field, its two initializers, and the `providers` vec build
in load_once; keep `edges` (used by index.commit_edges).

## Notes (no action)

- registry.rs:48: uid is allocated before parent validation, so a failed
  spawn burns a uid (gaps only; harmless; TS allocates at publication).
- fiber.rs disposables: eagerly-disposed effects leave None holes until the
  next full unload compacts; matches TS rebuild-on-unload; low severity.
- Registry keeps the plugin Arc alive, so plugin_key (address identity) has
  no ABA hazard; entries without live fibers linger until uninstall (same
  as TS registry).
- Drain ordering: consumers are drained at the provide-disposer's LIFO turn
  (service.rs:297-314), not strictly before other provider effects unwind;
  identical to TS disposer placement, safe (impl removed before consumers
  re-resolve). The CLAUDE.md phrasing is looser than the code; code is fine.

## Verified-good (kernel)
- omega-before-body, panic containment via spawned body + JoinError mapping,
  transactional rollback, stale-target pre-check, restart force semantics,
  fail_load parking, join/settle wakeup protocol (fiber.rs) - matches D4/TS.
- Two-level require distinction (context.rs:292-300) = TS Alg 6 mapping;
  strict resolution requires Active provider + check predicate
  (service.rs:104-124).
- Five dispatch modes match cordis: emit fire-and-forget, parallel
  aggregate, serial stop-at-bail, bail first-non-null (is_bailed =
  non-null, events.rs:60), waterfall skip-next-vetoes-chain-and-fallback.
  internal/dispatch recursion guard (events.rs:138); internal/service is
  label-filtered, global listeners bypass (events.rs:121-133).
- Scope chains: immutable persistent list, innermost-first label shadowing,
  root-most-first intercept layers (scope.rs + tests) = TS prototype chain.
- Cascade: child lifecycle is one parent effect (registry.rs:85-99, Eq. 31);
  initial check_impl loop before first refresh (registry.rs:105-107) closes
  the consumer-after-provider spawn race.
- Withdrawal protocol: impl removed -> notify -> join touched consumers
  (service.rs:297-314); provider self-visible during activation
  (commit_self + pre-body store commit).
