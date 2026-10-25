# dougong-abi

The stable C-ABI contract between dylib plugins and a dougong host (`#[repr(C)]`, zero dependencies). An independently compiled `.so` plugin needs only this crate: export the `DOUGONG_PLUGIN_EXPORTS` table, speak the caller-owned buffer protocol, and pass the `ABI_VERSION` gate.

Event chaining uses the CPS protocol (v2): `on_event` returns `STATUS_DEFERRED` to request the rest of the chain and the host re-enters through `on_next`. Any contract change must bump `ABI_VERSION` (design decision D14; see the repository's [design document](https://github.com/greatwallisme/dougong/blob/master/doc/research/dougong-design.md)).
