# Progress Log

## Session: 2026-10-20

### Current Status
- **Phase:** 1 - Requirements & Discovery
- **Started:** 2026-10-20

### Actions Taken
-

### Test Results
| Test | Expected | Actual | Status |
|------|----------|--------|--------|

### Errors
| Error | Resolution |
|-------|------------|

## Session: 2026-10-20（实现 Phase 1）
- rust-guidelines 技能已读（00 质量门 / 07 通用 / 04 FFI / 06 safety）
- workspace 脚手架：resolver 3、edition 2024、workspace lints（pedantic+nursery+restriction 全开）
- dougong-abi 完成：导出表/状态码/五模式常量/write_out/NextFn + 5 单测 + 1 doctest
- 修复过程：string_to_string lint 已被上游移除（删）；write_out 由 safe 改 unsafe fn（M-UNSOUND 真不健全）；cargo_common_metadata 以 publish=false+注释豁免（repository 待定）；两个 semicolon lint 冲突用 let () = 表达式位化解
- 验证：cargo fmt --check / clippy -D warnings / test (6) / doc -D warnings 全绿；git 首次提交完成

## Session: 2026-10-20（实现 Phase 2-3）
- Phase 2 完成：dougong 内核 3149 行（error/scope/plugin/events/service/fiber/context/registry/logger/lib）
  - commit 0da4357；cargo check 0 警告 0 错误
  - 过程：std Mutex → parking_lot（消灭 17 个 missing_panics_doc）；clippy --fix + 手工修完 86→0
  - 两个 restriction/pedantic lint 结构性互斥（semicolon_outside_block vs if_nothing_returned）→ 配置层裁决保留 pedantic
- Phase 3 进行中：tests/kernel_semantics.rs 7/7 通过（LIFO effects、事务回滚、五事件模式、isolate 路由、
  拦截合并、配置热更、父级联）；tests/acceptance.rs（论文 §15 映射：响应式接线、提供者身份语义、
  drain 排序、独立性、realm、capability 拒绝、失败恢复）编译中
- 捕获纪律教训：plugin() 的 Fn 闭包内不得移出捕获的 Arc —— 一律 clone-before-move

## Session: 2026-10-20（实现 Phase 2-3 完成）
- 内核 3149 行全绿提交（0da4357 → amend 3530199）
- 测试 24 项全绿：3 scope 单测 + 7 kernel_semantics + 7 acceptance + 1 doctest + abi 6
- 测试驱动修出 5 个真缺陷：ω 提交时序（TS _reload 前置）、初始 check_impl 缺失（晚生消费者永久 Pending）、
  restart force 三次激活、插件体 panic 杀死 driver（join 挂死→包容为 fail_load）、get 解析语义对齐 TS Alg6
- 质量门：fmt/clippy(-D warnings, pedantic+nursery+restriction)/test/doc 全 PASS
- 工程教训：管道 `cmd | tail` 吞退出码导致带病提交过一次（已 amend 修复）；Fn 闭包捕获 Arc 必须 clone-before-move

## Session: 2026-10-20（实现 Phase 4）
- dougong-dylib 完成：libloading + ABI 门 + descriptor(JSON) 解析 + DynPlugin 适配器
  - provide 服务以 Invoker（Invoke trait 动态通道，D6）暴露；teardown effect 先注册 → LIFO 在 provide 后运行
  - 句柄类型改 *mut c_void（C 惯例）；DylibInvoke unsafe Send/Sync 附内核串行化论证
- greeter-plugin cdylib 示例（纯 dougong-abi 依赖）+ dylib_demo 端到端验证通过
- dougong-abi 增 descriptor 导出；doctest 同步
- 质量门全绿：fmt/clippy(-D warnings)/34+ tests/doc；commit a7f3193
- 过程坑：ABI 结构体加字段后需同步 doctest 静态表与 fake 表；Fn 闭包捕获纪律再次生效

## Session: 2026-10-21（实现 Phase 5）
- WIT 契约落盘 wit/dougong-plugin.wit（dougong:kernel@0.1.0）；D7 落定差异记入设计文档：
  method→u32、mode 移到 dispatch、handle-event 带 next-token、载荷统一 bytes(JSON)、handle 0=teardown
- greeter-wasm guest：wit-bindgen 0.61 宏 + `cargo build --target wasm32-wasip2` 直构建（免装 cargo-component）
- dougong-wasm 宿主：bindgen!（imports/exports default:async，wasmtime48 已废顶层 async 选项）
  + fuel/epoch/StoreLimits 三重沙箱 + 最小 WASI（空 WasiCtx，wasip2 guest 运行时必声明 wasi:io）
- 实证 API 差异（勿凭记忆）：instantiate_async 返回单值非元组；根 Linker 是 core 模块用（组件须 component::Linker）；
  add_to_linker 需 HasSelf<T>；导入 trait 签名是扁平 WIT result（无 wasmtime::Result 外壳）；
  资源 Host trait 需实现 drop；ResourceTable 删除方法是 delete 不是 remove；WasiView::ctx 返回 WasiCtxView
- 真语义问题一枚：provider apply 持锁期间消费者被通知唤醒并调用服务 → try_lock 快速失败会永久 Failed。
  修复为「跨任务等待、同任务拒绝」（executing 标记记录持锁任务 id）
- wasm_demo 端到端全通：加载→apply→provide→消费者反应式接线→跨界调用→serial 事件→waterfall 拉链变换→
  drain→collected disposer→teardown（LIFO 顺序与 dylib 一致）
- 内核顺带修复：清掉 request_epoch 里已提交的 [probe] eprintln 调试残留；新增 fiber uid/epoch 观测 API

## Session: 2026-10-21（实现 Phase 6-7，交付完成）
- Phase 6 dougong-loader：JSON 期望态 + diff reconcile（insert/update/retire；isolate 变更=retire+insert）
  - 语义发现：ctx.isolate 每次 mint 新 label → loader 自持 scope→label 映射（同 realm 共 label）；
    内核补公共 Context::fresh_label 与 isolate_with_label 配套
  - builtin source：基座自带插件与外部 artifact 同一套声明式管理；文件监视=事件源，不在 loader（D9 论证）
  - 行为测试 4 项 + fingerprint 单测；loader_demo 三遍迭代（boot 热更 + greeter dylib + retire）
- Phase 7：README 完整化；终验 fmt/clippy(-D)/test(16 ok)/doc(-D) 全绿 + 三 demo exit 0
- 交付 commits：6ee6698(kernel fix) → 5475e7f(wasm) → a210731(loader) → 本提交(README+plan)
- 遗留可选项（应用层，非内核缺口）：能力衰减策略库、llmharness 本体、配置 include/patch 语法糖
