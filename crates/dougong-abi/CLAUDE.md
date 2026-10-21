# CLAUDE.md — dougong-abi

The zero-dependency C-ABI contract for dylib plugins. Deliberately depends on nothing — not even the kernel — so guest artifacts can be pure C/FFI crates.

## Rules

- Any change to `PluginExports` or the status/enum constants **breaks the ABI**: bump `ABI_VERSION` and update the doctest's static table plus `dougong-dylib`'s fake test tables in the same commit (they duplicate the export shape).
- Buffer protocol: callees write at most `cap` bytes into the caller buffer and report the used length through `len`; return `STATUS_CAPACITY` when it does not fit. Use `write_out` for this and never invent ad-hoc copies.
- `unsafe` blocks here carry SAFETY comments on both sides of each FFI assumption (host buffer contract, handle ownership). Keep that standard.
- Method selectors are `u32` (D6 dynamic channel) — the same convention the WIT world uses; do not switch to strings.
- Event pulls are CPS since v2 (D14): `on_event` returns `STATUS_DEFERRED` to request the waterfall continuation (any output written before deferring is the continuation payload; empty = original), and the host re-enters through `on_next` with `chain_status` OK/FAILED. `STATUS_DEFERRED` from `on_next` is a contract violation. The v1 synchronous `NextFn` is gone — do not resurrect it.
- The descriptor's `listen` map is contract *data*, not struct shape: extending it needs no `ABI_VERSION` bump; touching `PluginExports` or the status constants does.
