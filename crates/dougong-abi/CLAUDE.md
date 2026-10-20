# CLAUDE.md — dougong-abi

The zero-dependency C-ABI contract for dylib plugins. Deliberately depends on nothing — not even the kernel — so guest artifacts can be pure C/FFI crates.

## Rules

- Any change to `PluginExports` or the status/enum constants **breaks the ABI**: bump `ABI_VERSION` and update the doctest's static table plus `dougong-dylib`'s fake test tables in the same commit (they duplicate the export shape).
- Buffer protocol: callees write at most `cap` bytes into the caller buffer and report the used length through `len`; return `STATUS_CAPACITY` when it does not fit. Use `write_out` for this and never invent ad-hoc copies.
- `unsafe` blocks here carry SAFETY comments on both sides of each FFI assumption (host buffer contract, handle ownership). Keep that standard.
- Method selectors are `u32` (D6 dynamic channel) — the same convention the WIT world uses; do not switch to strings.
