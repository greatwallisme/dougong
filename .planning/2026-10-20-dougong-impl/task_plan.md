# Task Plan: dougong（斗拱）实现

## Goal
实现 dougong：cordis 的 Rust 内核 + dylib/so 与 wasm 双插件形态 + 配置 reconcile，单一代码库整体交付（无版本分期），零警告零错误。

## 设计基准
doc/research/dougong-design.md v2（D4-D13 决策 + R1-R3 裁决）：tokio；混合键（进程内类型键 / 跨边界 service-key{interface,abi-hash}）；完整级联；driver-loop 迁移；dylib teardown-only 无热替换；反向依赖索引；事件五模式；WIT world 草图；Linux 一等 + Win/macOS + musl；edition 2024。
验收：论文 §15 十场景 + jincdream 163 场景矩阵映射选集；cargo test/clippy/fmt 全绿。

## Next Step
Phase 5：dougong-wasm（WIT world 落盘 + wasmtime bindgen 宿主 + 沙箱限额 + wasm 插件示例）。

## Current Phase
Phase 5

## Phases

### Phase 1: 脚手架 + dougong-abi
- [x] 调用 Skill(rust-guidelines)；workspace Cargo.toml（含严格 lints）
- [x] dougong-abi：#[repr(C)] 导出表 + ABI_VERSION + 缓冲区协议 + 五模式常量（零依赖，fmt/clippy/test/doc 全绿）
- **Status:** complete

### Phase 2: dougong 内核
- [x] error / context+scope 链 / registry / service store+双索引 / events 五模式 / fiber（epoch+driver+级联+ω 提交/panic 包容）
- [x] commit 0da4357 + 3530199（amend）；内核 ~3.1k 行
- **Status:** complete

### Phase 3: 语义验收测试
- [x] tests/kernel_semantics.rs 7 项（dispose/events/isolate/update/cascade 等）
- [x] tests/acceptance.rs 7 项（论文 §15 映射：响应式接线/身份语义/drain 排序/独立性/realm/capability 面/失败恢复）
- [x] 测试驱动修出 5 个真缺陷（ω 提交时序、初始 check_impl、restart force、panic 包容、get 解析语义）
- **Status:** complete

### Phase 4: dylib 宿主 + 示例
- [x] dyn-host 适配器（libloading + ABI 门 + DynPlugin）
- [x] greeter cdylib 示例 + dylib_demo 端到端验证
- **Status:** complete（commit a7f3193）

### Phase 5: dougong-wasm
- [x] WIT world `wit/dougong-plugin.wit`（dougong:kernel@0.1.0，D7 落定）落盘，wit-bindgen 0.61.1 校验通过
- [x] greeter-wasm guest 组件（wit-bindgen 宏 + wasm32-wasip2 直构建，无需 cargo-component）
- [x] dougong-wasm 宿主（bindgen! async + fuel/epoch/StoreLimits + 最小 WASI + DynPlugin 同构适配器）
- [x] wasm_demo 端到端验证（服务调用 + serial 事件 + waterfall 拉链 + drain + teardown）
- [x] 质量门终验（fmt/clippy -D warnings/test 全量/doc -D warnings）+ 提交
- **Status:** complete

### Phase 6: dougong-loader
- [x] 声明式配置（JSON 期望态）+ diff reconcile（insert/update/retire/isolate 迁移）
- [x] scope→label 映射（同 realm 共 label）+ builtin source + 内核补 Context::fresh_label
- [x] 行为测试 4+1 项 + loader_demo 端到端 + 四门全绿
- **Status:** complete

### Phase 7: 全量验证
- [ ] cargo test / clippy -D warnings / fmt 全绿；README；示例可运行验证
- **Status:** pending

## Decisions Made
| Decision | Reason |
|----------|--------|
| 全部模块一个代码库交付 | 用户明确批评分期（体量仅数千行） |
| rust-guidelines 先行 | 技能自述强制（写 Rust 前必调） |
| R1-R3 裁决落档 | 见 dougong-design.md §6 |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| init 落 doc/research 且 slug 冲突 | 1 | 迁移改名 2026-10-20-dougong-impl 至项目根 |
