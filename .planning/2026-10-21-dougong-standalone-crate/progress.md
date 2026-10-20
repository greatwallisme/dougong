# Progress log

## Session 2026-10-21
- Resolved plan selection: created named plan `2026-10-21-dougong-standalone-crate`
  (multiple plans existed; hook reported nothing injected). Prior plan
  `2026-10-20-dougong-impl` is 6/7 complete - untouched.
- `git diff --stat`: clean tree (skill commit 8501cdf is HEAD).
- Audited root Cargo.toml + all 7 member manifests (evidence E1/E2).
- Probed cordis ecosystem repos: dshbox__cordis-rs, bayunai__cordis-rs,
  dsh-rs (evidence E3 - dsh-rs consumes crates.io `cordis-rust` 0.2).
- Read dougong-design.md sections 3/4/6: R1-R3 adjudicated; delivery
  philosophy "one codebase, no phasing" (evidence E4/E5).
- Phase 1 complete, Phase 2 complete: recommendation = YES, standalone crate
  set in one workspace. Delivered to user in chat; awaiting adoption.
- No errors encountered; no code changed (decision task - Phase 3/4 are the
  execution phases and are gated on user adoption + canonical repo URL).

## Session 2026-10-21 (ruling R4)
- User ruled: the pinned toolchain is absolute - never build repo artifacts
  with another toolchain. My cross-version experiment had left a 1.96-built
  .so in target/debug; first rebuild attempt was a no-op (fresh fingerprints
  masked the swapped file), so `cargo clean -p dougong-abi -p greeter-plugin`
  + pinned workspace rebuild was required. Hash now differs from the 1.96
  artifact, demo exit 0, experiment temp copies deleted.
- R4 recorded in doc/research/dougong-design.md section 6; Phase 3 item 6
  rewritten: MSRV is declared-only; local cross-toolchain verification
  removed from the plan (isolated CI only, if ever).

## Session 2026-10-21 (R4 amendment: MSRV declaration removed)
- User ruled the 1.85 wording is an unevidenced claim (never compiled on
  that toolchain) and ordered it deleted. Removed `rust-version` from the
  root `[workspace.package]`, `rust-version.workspace = true` from all 7
  member manifests, the MSRV references in rust-toolchain.toml comments,
  CLAUDE.md, README, and the R4 record; planning evidence updated to match.
  study-3's "MSRV 1.85" line is a third-party project's own README quote -
  research record, kept.
- Verified: `cargo build --workspace` exit 0 (manifest parse intact).
