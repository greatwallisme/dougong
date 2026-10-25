// Rust guideline compliant 2026-10-25
//! Plugin resource budgets (performance track P7, improve 一.7).
//!
//! Every budget is optional; `None` (the default) preserves the unbounded
//! pre-P7 behavior, so an existing host observes zero change until it opts in
//! through [`ContextOptions::budgets`]. The mechanism lives in the kernel; the
//! cap values are host wiring. The two caps cover the resources that cross a
//! kernel choke point for every plugin form; compute metering stays a sandbox
//! concern (wasm fuel/epoch/`StoreLimits`), because trusted dylib and
//! in-process code has no kernel-side enforcement point.
//!
//! [`ContextOptions::budgets`]: crate::ContextOptions::budgets

/// Plugin resource budgets (P7).
///
/// All fields default to `None` (unbounded, the pre-P7 behavior).
#[derive(Debug, Clone, Default)]
pub struct BudgetConfig {
    /// Caps the number of *live* cleanup-bearing effects per fiber.
    ///
    /// Every plugin-facing resource registration funnels through
    /// `Fiber::register_effect` — `provide`, `accessor`, `on`, `once`,
    /// `on_dispatch`, and plain teardown effects — so this one cap bounds
    /// them all (a listener registration is itself a `Critical` effect).
    /// A breach returns a `budget_exceeded` error: during activation the
    /// body fails and the load rolls back (`fail_load`); at runtime the
    /// error returns to the registering call. Kernel cleanup only *runs*
    /// existing disposers and never registers new effects, so a cap cannot
    /// interrupt teardown.
    pub max_effects: Option<u32>,
    /// Caps the events one fiber may emit per activation window (the window
    /// resets every time the fiber's body starts).
    ///
    /// Enforcement sits on the plugin-facing emit APIs; kernel-protocol
    /// `internal/*` emissions (`emit_internal`) are exempt so a plugin
    /// budget cannot sever the withdrawal protocol's event propagation.
    /// A breach drops that emission, logs once per window through
    /// [`ErrorPolicies`](crate::ErrorPolicies) (`Logic` class), and drops
    /// further breaches silently; `emit_with_policy` reports
    /// [`EmitOutcome::Dropped`](crate::EmitOutcome) for a refused emission.
    /// Under concurrent emitters up to `concurrency - 1` extra events may
    /// pass at the boundary (bounded soft overshoot, unlike the strict
    /// [`BudgetConfig::max_effects`] check).
    pub max_event_emits: Option<u64>,
}
