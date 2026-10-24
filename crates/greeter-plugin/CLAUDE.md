# CLAUDE.md — greeter-plugin

Example dougong plugin as a pure C-ABI cdylib. Depends **only** on `dougong-abi` — no kernel, no tokio, no serde cross the boundary. It is the reference for writing dylib plugins by hand and the live artifact for `dougong-dylib`'s demo and `dougong-loader`'s demo.

## Rules

- The export table must stay in lockstep with `dougong-abi`: any struct change there means updating `DOUGONG_PLUGIN_EXPORTS` here (and the doctest/fake tables in the other two crates) in the same commit.
- Adding a method: pick the next `u32` selector constant, handle it in `call`, and document it in this file's doc comment. Payload encoding is method-specific opaque bytes.
- Outbound surface (ABI v3): `setup` copies the `HostExports` vtable and spawns the worker thread, which requires the declared `greeter.oracle` service and emits `greeter/ready`; `GREETER_WORKER_REPLY` reads the reply back, and `GREETER_REQUIRE` issues an on-demand require from the call handler (`name\0payload` in, `i32 LE status || reply` out). The descriptor's `inject` therefore contains `greeter.oracle` — every demo world that loads greeter (dylib_demo, loader_demo, harness_demo) must provide it or the greeter fiber parks in `Pending`.
- Plugin-owned threads are not covered by the host's per-instance lock: state shared with the worker carries its own mutex, and `teardown` must join the worker before returning (thread discipline, A6).
- Event surface (ABI v2, D14): `descriptor.listen` maps event names to u32 ids (`GREETER_EV_PING`/`GREETER_EV_UPPER`); `on_event` switches on the id, and `upper` demonstrates the CPS pull — `STATUS_DEFERRED` with `next_available == 1`, then upper-case the chain reply in `on_next`. Adding a listened event: pick the next u32 event id, extend `listen` + the `on_event` switch, and document it here.
- Teardown (`run_disposer` equivalent here is the `teardown` export) must never fail the process — best-effort stderr writes only.
- Host-target builds of this crate produce the real `.so` used by the demos; keep `cargo build --workspace` able to produce it.
