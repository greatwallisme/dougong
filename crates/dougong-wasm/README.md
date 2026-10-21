# dougong-wasm

dougong 的 wasm 组件插件宿主（wasmtime 48）：fuel 硬预算 + epoch 协作式让步 +
`StoreLimits` 资源帽 + 能力清空的 WASI。WIT 契约单一真源为本 crate 内的
`wit/dougong-plugin.wit`（world `dougong:kernel@0.1.0`，设计决策 D7/D14）。

配套示例：`examples/wasm_demo.rs`（需先以 `--target wasm32-wasip2` 构建
[greeter-wasm](../greeter-wasm) 宿主侧组件）。
