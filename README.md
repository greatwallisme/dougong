# dougong（斗拱）

cordis 的 Rust 实现：时间可组合 × 空间可组合的通用插件基座内核。

- **时间可组合**：每一次对环境的变更都是可逆 effect（LIFO 收集，卸载即按注册逆序回滚，teardown 由 setup 推导）
- **空间可组合**：依赖显式声明、按可用性响应式解析——提供者晚到/先走/被替换时，消费者自动重接线
- **插件形态**：`dylib/so` 动态库（C-ABI，可信代码）与 `wasm` 组件（沙箱，能力最小化）
- **研究文档**：[doc/research/](doc/research/)（[cordis.md](doc/research/cordis.md) 论文全文分析、TS 版对照、生态调研、[设计决策与裁决记录](doc/research/dougong-design.md)）。

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

# 三个机制示例（每种边界各一个）
cargo run -p dougong-dylib  --example dylib_demo    # dylib 边界：响应式接线 + 调用 + drain
cargo run -p dougong-wasm   --example wasm_demo     # wasm 边界：调用 + 事件 + waterfall + drain
cargo run -p dougong-loader --example loader_demo   # 声明式配置：insert/update/retire

# 多插件组合示例（对齐 deepseek-harness 的"一切皆插件"形态）
cargo run -p dougong-loader --example harness_demo  # 9 槽混合来源（builtin+dylib+wasm）单一文档、
                                                    # 跨插件依赖注入、能力 seam 配置换供应商、
                                                    # 策略 guard 经 waterfall 否决、 Failure 隔离
```

## 在宿主项目中用 AI 开发 dougong 插件

dougong 以 crate 形态被宿主项目引入（`dougong` / `dougong-dylib` / `dougong-wasm` / `dougong-loader`）。本仓库为**下游项目**的 AI 编码准备了现成技能：[`skills/dougong-plugin-dev/`](skills/dougong-plugin-dev/)。把该目录复制到宿主项目的 `.claude/skills/` 下（Claude Code 等工具会自动发现项目级技能），AI 即可按 dougong 的真实契约开发插件——形态选型、dylib 导出表与描述符、v2 CPS 事件协议、wasm 沙箱约束、验证命令与常见陷阱——而不是凭通用知识猜 API。

## 插件开发工作流

四个动作贯穿插件的一生（可运行对照：[`crates/dougong-loader/examples/harness_demo.rs`](crates/dougong-loader/examples/harness_demo.rs) + 同目录的 [`harness.json`](crates/dougong-loader/examples/harness.json)）：

**1. 开发** —— 选一种形态，只写"声明 + 注册"，不写卸载逻辑：

- 进程内（最简）：`dougong::plugin("名字", 依赖列表, |ctx, 配置| ...)`，激活体里用 `ctx.provide("服务名", 值)` 注册能力；
- dylib：cdylib crate，只依赖 `dougong-abi`，写导出表（照 `greeter-plugin`）；
- wasm：wit-bindgen 实现 Guest trait（照 `greeter-wasm`），跑在沙箱里。

**2. 加载** —— 写一份 JSON 期望态文档（每个插件一行：名字 / 来源 `builtin`·`dylib`·`wasm` / 配置 / 隔离域，见 [`harness.json`](crates/dougong-loader/examples/harness.json)），宿主加载并收敛：

```rust
let mut reconciler = Reconciler::new(内置插件表, wasm宿主);
reconciler.replace(Config::load_file("harness.json")?);
reconciler.apply(&ctx).await;      // 文档里的所有插件自动启动
```

**3. 声明依赖** —— 插件用 `inject` 列出需要的服务名，其余交给内核：提供者没上线就停在 `Pending`，上线即自动激活（文档行序无关）；不声明就用会被直接拒绝（能力检查）；提供者被替换，消费者自动重接线。

**4. 卸载** —— 从文档里**删掉那一行**，重新 `replace` + `apply`：该插件被精确卸载，它的消费者先被 drain，注册的资源按 LIFO 自动回滚——不需要手写任何卸载代码。代码路径等价物：`handle.dispose()` / `ctx.uninstall(&插件)` / `ctx.stop()`。

例外说明：dylib 的代码镜像进程内不卸载（换 .so 版本 = 重启进程）；其配置可随时 `handle.update(新配置)` 热更，不受影响。

## 内核一句话

插件声明依赖（`inject`）并在激活体（`apply`）里只经 `ctx.effect`/`ctx.provide` 修改环境；
依赖未就绪时 fiber 停在 `Pending`，提供者集一变即按 epoch 收敛（迁移/重激活）；
提供者被卸载时先 drain 消费者再回滚自身 effect——顺序由 LIFO 保证。

## 信任边界与沙箱

| 形态 | 信任模型 | 边界 |
|------|----------|------|
| 进程内 Rust | 完全可信 | 无 |
| `dylib/so` | 可信代码（[设计决策 D5](doc/research/dougong-design.md)：只 teardown、不卸载镜像） | C-ABI 契约 + ABI 版本门 |
| `wasm` 组件 | 不可信 | Component Model world 即全部权威面；零能力 WASI；fuel 硬预算 + epoch 协作式让步 + `StoreLimits` 内存/实例帽 |

跨边界服务统一走动态通道（[设计决策 D6](doc/research/dougong-design.md)）：`service-key{name, abi-hash} + method(u32) + bytes`，
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

## 参考与致谢

dougong 站在这些开源项目的肩膀上（完整证据链见 [doc/research/study-1..4](doc/research/)）：

| 项目 | 参考了什么 |
|------|-----------|
| [cordiverse/cordis](https://github.com/cordiverse/cordis)（TS 原版）与 [cordiverse/paper](https://github.com/cordiverse/paper)（arXiv:2608.25512，PKU / DeepSeek-AI） | 语义源：可逆 effect、响应式 coeffect、epoch 收敛、五事件模式；dougong 是其语义的 Rust 复现（见下节对齐程度） |
| [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（dsh） | "一切皆插件"的应用架构参照：capability seam、isolate realm 给会话不同能力集、事件即扩展点——见 [`harness_demo`](crates/dougong-loader/examples/harness_demo.rs) |
| [redoop/cordis-rust](https://github.com/redoop/cordis-rust) | inverted-API 模式先例（插件侧只计算、宿主持有全部框架语义） |
| [jincdream/cordis-rs](https://github.com/jincdream/cordis-rs) | 语义对齐清单（study-2），`kernel_semantics.rs` 的对照基准 |
| [cordis_wasm](https://github.com/cordiverse/cordis)、[nucleus](https://github.com/meritozh/nucleus) | Component Model 路线（[设计决策 D13](doc/research/dougong-design.md) 实证）与 WASI 0.3 对照 |
| dshbox/bayunai/helloworldtang/collab-ai-dev 等 cordis-rs 变体、tiny-cordis、spatiotemporal、cordis-verus、metis | 生态调研与设计取舍证据（study-3/4） |

## 与原版 cordis 的对齐程度

- **语义对齐（复现并有测试钉住）**：可逆 effect 与 LIFO 回滚、epoch 驱动的收敛/重激活、两级依赖解析检查（TS Algorithm 6，`inactive_dependency` vs `undeclared_dependency`）、事件五模式（`isBailed` 非空即断、waterfall 跳过 `next` 即否决、`internal/*` 递归保护）、isolate 域与 scope 遮蔽、provider 卸载先 drain 消费者、声明式期望态 reconcile（insert/update/retire/失败隔离）、插件 body panic 隔离为加载失败。验收映射：`tests/acceptance.rs`（论文 §15 场景）、`tests/kernel_semantics.rs`（上游兼容面）。
- **dougong 特有的技术折中（原版无需面对）**：原版是 TS 架构，插件由语言运行时装载进同一进程，边界天然稳定，不需要任何 ABI。dougong 的三个形态里有两个跨越独立编译产物，而 Rust 恰恰没有跨产物的稳定 ABI——"三种形态共享一个语义契约"因此只能靠**显式定义边界契约**达成，这是折中而非加成：dylib 侧付出 C-ABI v2 契约与缓冲协议/显式序列化（事件续链还得用[设计决策 D14](doc/research/dougong-design.md) 的 CPS 方案绕开"同步 C 回调无法 await"）；wasm 侧付出沙箱工程（capability-empty WASI + fuel/epoch/内存帽）；两个边界都引入每实例串行化约束。进程内形态则与原版一样，不付这些代价。
- **有意不对齐**：Node 专属机制（prototype context、pnpm/bundle/HMR 生态）不做平移；capability 策略库属应用层（dsh 语境），不进内核；`cordis.patch.yml` 的有序分层由"整文档替换 + 响应式收敛"替代——结果只取决于最终文档，歧义靠硬拒而非层序仲裁（dougong 不需要层序；完整论证见 [doc/research/dougong-design.md](doc/research/dougong-design.md) 的设计决策 **D9**「声明式配置与 loader 的真实切分」）。
- **文档资产来源**：[cordis.md](doc/research/cordis.md) 是第三方撰写的论文全文分析（论文本体 arXiv:2608.25512，PKU/DeepSeek-AI），获取自 [acryldev/cordis-meta-framework-spatiotemporal-composability](https://github.com/acryldev/cordis-meta-framework-spatiotemporal-composability)，存档作研究参考，非本项目作品——详见该文件内的来源标注。
