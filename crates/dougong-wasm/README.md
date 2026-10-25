# dougong-wasm

The wasm component plugin host for dougong (wasmtime 48): a hard fuel budget + cooperative epoch yielding + `StoreLimits` resource caps + a capability-empty WASI. The single source of the WIT contract is this crate's `wit/dougong-plugin.wit` (world `dougong:kernel@0.1.0`; design decisions D7/D14).

Companion example: `examples/wasm_demo.rs` (build the [greeter-wasm](https://github.com/greatwallisme/dougong/blob/master/crates/greeter-wasm) guest with `--target wasm32-wasip2` first).
