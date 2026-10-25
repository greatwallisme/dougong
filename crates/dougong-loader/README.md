# dougong-loader

The declarative configuration and reconcile layer for dougong: one JSON expected-state document describes the full plugin set (mixing the `builtin` / `dylib` / `wasm` sources), and `Reconciler::apply` converges the live set by diff — insert / config hot-update (update) / retire the absent / isolate failures, idempotently and independent of line order. Scopes (`isolate`) share one isolation label per name.

For the full multi-plugin composition and provider-swap walkthrough: `examples/harness_demo.rs` + `harness.json` / `harness.upper.json`.
