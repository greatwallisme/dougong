# dougong-abi

The stable C-ABI contract between dylib plugins and a dougong host (`#[repr(C)]`, zero dependencies). An independently compiled `.so` plugin needs only this crate: export the `DOUGONG_PLUGIN_EXPORTS` table, speak the caller-owned buffer protocol, and pass the `ABI_VERSION` gate.

Event chaining uses the CPS protocol (v2): `on_event` returns `STATUS_DEFERRED` to request the rest of the chain and the host re-enters through `on_next`. ABI v4 adds `HostExports.grant`: a provider can delegate `require` access to one owned service for one committed target activation, with the grant expiring alongside the granter activation. Any contract change must bump `ABI_VERSION` (design decisions D14 and D31; see the repository's [design document](https://github.com/greatwallisme/dougong/blob/master/doc/research/dougong-design.md)).
