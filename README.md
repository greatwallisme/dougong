# dougong（斗拱）

cordis 的 Rust 实现：时间可组合 × 空间可组合的通用插件基座内核。

- 内核：context / fiber / 依赖注入 / 可逆 effect / 响应式通知 / reconcile
- 插件形态：`dylib/so` 动态库（C-ABI）与 `wasm` 组件
- 研究文档：[doc/research/](doc/research/)（论文精读、TS 版对照、生态调研、设计决策）

## Crates

| crate | 内容 |
|-------|------|
| `dougong` | 内核 |
| `dougong-abi` | dylib 插件 C-ABI 契约（零依赖） |
| `dougong-wasm` | wasmtime 组件宿主 |
| `dougong-loader` | 声明式配置与 reconcile |
