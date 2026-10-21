# dougong-loader

dougong 的声明式配置与 reconcile 层：一份 JSON 期望态文档描述全部插件
（`builtin` / `dylib` / `wasm` 三种来源混用），`Reconciler::apply` 对活集做
diff 收敛——insert / 配置热更（update）/ 缺席 retire / 失败隔离，幂等且与
行序无关。作用域（`isolate`）按名字共享同一隔离标签。

多插件组合与换供应商的完整演示：`examples/harness_demo.rs` +
`harness.json` / `harness.upper.json`。
