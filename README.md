# dougong（斗拱）

cordis 的 Rust 实现：时间可组合 × 空间可组合的通用插件基座内核。

- **时间可组合**：每一次对环境的变更都是可逆 effect（LIFO 收集，卸载即按注册逆序回滚，teardown 由 setup 推导）
- **空间可组合**：依赖显式声明、按可用性响应式解析——提供者晚到/先走/被替换时，消费者自动重接线
- **插件形态**：`dylib/so` 动态库（C-ABI，可信代码）与 `wasm` 组件（沙箱，能力最小化）
- **研究文档**：[doc/research/](doc/research/)（[cordis.md](doc/research/cordis.md) 论文全文、TS 版对照、生态调研、[设计决策](doc/research/dougong-design.md) D4-D13）

## Crates

| crate | 内容 |
|-------|------|
| `dougong` | 组合内核：context / fiber / 依赖注入 / 可逆 effect / 响应式通知 / 事件五模式 / 反向依赖索引 |
| `dougong-abi` | dylib 插件 C-ABI 契约（零依赖） |
| `dougong-dylib` | dylib/so 宿主：libloading + ABI 门 + 动态通道适配 |
| `dougong-wasm` | wasm 组件宿主：wasmtime + fuel/epoch/内存限额 + 最小 WASI |
| `dougong-loader` | 声明式 JSON 期望态 + diff reconcile（insert/update/retire/isolate） |
| `greeter-plugin` | dylib 插件示例（纯 C-ABI cdylib） |
| `greeter-wasm` | wasm 插件示例（wit-bindgen，`wasm32-wasip2`） |

## 快速开始

要求：Rust（edition 2024）；开发与验证以 `rust-toolchain.toml` 钉定的工具链为准，不另行宣称兼容下限；wasm 示例另需 `rustup target add wasm32-wasip2`。

```sh
# 宿主侧全量构建 + 测试
cargo build --workspace
cargo test --workspace

# 构建 wasm guest 示例组件（greeter-wasm）
cargo build -p greeter-wasm --target wasm32-wasip2

# 三个端到端示例
cargo run -p dougong-dylib  --example dylib_demo    # dylib 边界：响应式接线 + 调用 + drain
cargo run -p dougong-wasm   --example wasm_demo     # wasm 边界：调用 + 事件 + waterfall + drain
cargo run -p dougong-loader --example loader_demo   # 声明式配置：insert/update/retire
```

## 内核一句话

插件声明依赖（`inject`）并在激活体（`apply`）里只经 `ctx.effect`/`ctx.provide` 修改环境；
依赖未就绪时 fiber 停在 `Pending`，提供者集一变即按 epoch 收敛（迁移/重激活）；
提供者被卸载时先 drain 消费者再回滚自身 effect——顺序由 LIFO 保证。

## 信任边界与沙箱

| 形态 | 信任模型 | 边界 |
|------|----------|------|
| 进程内 Rust | 完全可信 | 无 |
| `dylib/so` | 可信代码（D5：只 teardown、不卸载镜像） | C-ABI 契约 + ABI 版本门 |
| `wasm` 组件 | 不可信 | Component Model world 即全部权威面；零能力 WASI；fuel 硬预算 + epoch 协作式让步 + `StoreLimits` 内存/实例帽 |

跨边界服务统一走动态通道（D6）：`service-key{name, abi-hash} + method(u32) + bytes`，
消费方按声明解析、未声明即拒绝（两级解析错误，对齐 TS Algorithm 6）。

## 质量门

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # pedantic + nursery + restriction
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

## 版本锚

- 内核/宿主：`wasmtime` 48.x（48.0.7 实证）、`wit-bindgen` 0.61.x（0.61.1 实证）、tokio、edition 2024
- WIT 契约：`wit/dougong-plugin.wit`（package `dougong:kernel@0.1.0`）——wasm 边界的单一真源
