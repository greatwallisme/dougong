# dougong

cordis 风格的组合内核（Rust 实现）：时间可组合（一切环境变更是可逆 effect，LIFO 回滚）× 空间可组合（声明式依赖、响应式解析）。

- 插件声明 `inject` 依赖并在 `apply` 中只经 `ctx.effect` / `ctx.provide` 修改环境；teardown 由注册自动推导
- 依赖未就绪时 fiber 停在 `Pending`，提供者集合一变即按 epoch 收敛重激活
- 事件五模式在派发时选择（emit / parallel / serial / bail / waterfall）
- 进程内 Rust 插件形态（同语义另有 [dylib](../dougong-dylib) 与 [wasm](../dougong-wasm) 边界形态，及声明式加载器 [dougong-loader](../dougong-loader)）

总览见[仓库根 README](https://github.com/greatwallisme/dougong)。
