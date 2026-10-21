# Findings: deepseek-harness patterns -> dougong mapping

Source: `~/project/reference/cordis/deepseek-harness/docs/architecture.md`
(TS/pnpm monorepo; cordis-based; ~45 packages; every part of the product is a
plugin).

## Extracted patterns

1. **Everything is a plugin.** Model adapter, tool registry, session log,
   and the agent loop itself are plugins contributing services - each
   replaceable from configuration. "There is no privileged core to patch."
2. **Capability seams** = Service Definition + Service Provider + Consumer.
   "One provider swap changes the whole product" (filesystem+subprocess share
   one execution world; pointing them at a remote sandbox moves Bash/PTY/LSP).
3. **Config-as-composition**: profiles list ordered bundles; patches replace
   config rows by id; layers stack. HMR watches and reloads.
4. **Isolate realms give sessions different capability sets**: "give one
   session a different capability set -> compose an agent preset; a service
   row there needs an `isolate` realm."
5. **Events are the extension points**, three domains: durable session facts,
   live `agent/*` events, and capability events (`fs/*`, `tools/*`,
   `telemetry/*`) that attach policy/adapters to a seam without importing the
   loop. Waterfalls for interception (`agent/pre-step`, `agent/request`,
   `llm/stream`, `tools/*` - listeners MUST call next to delegate); serial
   for `agent/turn-stopping`.
6. **Turn flow**: agent-loop plugin drives turn/start -> pre-step waterfall ->
   request -> llm/stream -> tool calls (pre-execute waterfall) -> step/end ->
   turn/end.

## dougong mapping used by the example (as implemented)

| dsh pattern | dougong mechanism |
|---|---|
| every part is a plugin | in-process plugins via `plugin()` closures in one example |
| service seam + provider swap | the `llm` row's CONFIG selects the adapter (`{"adapter": "echo"|"upper"}`); a config-only document change hot-restarts the row and consumers re-resolve reactively |
| config rows / patches | `Reconciler::replace` + `apply`; desired state is order-independent (reactive resolution) - no ordered layers needed |
| waterfall interception | `tools/pre-execute` guard veto dispatched by the agent plugin |
| capability event without importing the loop | guard plugin injects NOTHING; attaches policy purely via event registration |
| mixed sources in one composition | one document: 7 builtin rows + dylib greeter + wasm greeter |

## Gap discovered while implementing (design question, not fixed here)

**Loader `isolate` scope does not namespace member services.** Two rows in
DIFFERENT scopes providing the SAME service name collide at the default
label (`duplicate_provide`): `ScopeNode::isolated(parent, scope_name, label)`
keys the override by the scope string, while `effective_label` looks services
up by name - so the realm label applies only to a service literally named
like the scope. The loader's `isolate` therefore groups rows onto a shared
label without giving members per-realm service namespaces. Consequence: dsh's
"give one session a different capability set via an isolate realm + same
seam" is NOT expressible through the loader today; the example achieves the
seam swap through config selection instead. If per-realm same-name capability
sets are wanted, that is a kernel/loader semantic decision (e.g. a scope-wide
default label in `ScopeNode` consulted by `effective_label`) and needs a
D-record. Verified empirically during harness-demo development (collision
observed; the same_scope test only proves join-on-one-label wiring, which
also works without any isolate).
