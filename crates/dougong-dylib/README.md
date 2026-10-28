# dougong-dylib

The dylib/so plugin host for dougong: `libloading` mapping + the ABI version gate + the dynamic channel adapter, turning C-ABI plugins into kernel `Plugin`s. The host serializes every foreign call through a per-instance lock; plugins mirror in-process residency (teardown-only, never `dlclose` — design decision D5/R1).

Since ABI v3 the boundary is bidirectional: a plugin may synchronously require other plugins' services and emit events from any of its own threads, with wait-graph reentrancy protection that turns require cycles into deterministic `reentrancy_cycle` errors instead of deadlocks. ABI v4 adds activation-scoped grants, allowing a provider to authorize a committed target activation to require one owned service without a static descriptor injection.

Companion examples: `examples/dylib_demo.rs` (the greeter full chain) and `examples/outbound_demo.rs` (ABI v3/v4 outbound acceptance: own-thread require/emit, an authorized grant callback, plus the permission-denial, reentrancy-cycle and cross-instance-cycle arms). For the trust model and the sandbox comparison see the [repository README](https://github.com/greatwallisme/dougong).
