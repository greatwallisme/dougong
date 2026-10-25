# dougong

A Rust implementation of a cordis-style composition kernel: temporal composability (every environmental mutation is a revertible effect, unwound LIFO) x spatial composability (declared dependencies, reactive resolution).

- Plugins declare `inject` dependencies and mutate the environment only through `ctx.effect` / `ctx.provide` inside `apply`; teardown is derived from registration
- A fiber parks in `Pending` until its dependencies are ready; any provider change reconverges and reactivates it along the epoch
- The five event modes are chosen at dispatch time (emit / parallel / serial / bail / waterfall)
- This is the in-process Rust plugin form; the same semantics also have the [dylib](https://github.com/greatwallisme/dougong/blob/master/crates/dougong-dylib) and [wasm](https://github.com/greatwallisme/dougong/blob/master/crates/dougong-wasm) boundary forms plus the declarative loader [dougong-loader](https://github.com/greatwallisme/dougong/blob/master/crates/dougong-loader)

For an overview see the [repository README](https://github.com/greatwallisme/dougong).
