# CLAUDE.md — greeter-plugin

Example dougong plugin as a pure C-ABI cdylib. Depends **only** on `dougong-abi` — no kernel, no tokio, no serde cross the boundary. It is the reference for writing dylib plugins by hand and the live artifact for `dougong-dylib`'s demo and `dougong-loader`'s demo.

## Rules

- The export table must stay in lockstep with `dougong-abi`: any struct change there means updating `DOUGONG_PLUGIN_EXPORTS` here (and the doctest/fake tables in the other two crates) in the same commit.
- Adding a method: pick the next `u32` selector constant, handle it in `call`, and document it in this file's doc comment. Payload encoding is method-specific opaque bytes.
- Event surface (ABI v2, D14): `descriptor.listen` maps event names to u32 ids (`GREETER_EV_PING`/`GREETER_EV_UPPER`); `on_event` switches on the id, and `upper` demonstrates the CPS pull — `STATUS_DEFERRED` with `next_available == 1`, then upper-case the chain reply in `on_next`. Adding a listened event: pick the next u32 event id, extend `listen` + the `on_event` switch, and document it here.
- Teardown (`run_disposer` equivalent here is the `teardown` export) must never fail the process — best-effort stderr writes only.
- Host-target builds of this crate produce the real `.so` used by the demos; keep `cargo build --workspace` able to produce it.
