# Changelog

本仓库的显著变更记录。版本语义：0.x 系列的 minor bump 即可含破坏性变更。
设计裁决全文见 [doc/research/dougong-design.md](doc/research/dougong-design.md)。

## [0.3.0] - 2026-10-24

### ABI v3：dylib 插件出站能力（破坏性变更）

dylib 边界由单向（宿主 → 插件）变为双向：插件可以从**任意自有线程**同步调用
其他插件的服务、向内核总线发射事件。裁决记录见设计文档 D16（D-V3-1/3/4/5）。

v2 → v3 diff 清单：

- `setup` 签名：`setup() -> *mut c_void` →
  `setup(host: *const HostExports) -> *mut c_void`。`HostExports` 指针仅在
  setup 调用期间有效；插件按值复制其三个字段（`host_handle` / `require` /
  `emit`）后，复制件在本次激活的 `teardown` 返回前可调用。
- 新增 `HostExports` vtable（dougong-abi）：
  - `require`：同步阻塞跨插件服务调用（service 名 + method + 字节载荷；in
    载荷不设上限，out 沿用 growable 缓冲协议与 16 MiB 界）。能力检查基准 =
    请求方 descriptor `inject` 集；解析绑定该激活的 realm。
  - `emit`：事件发射，入队即返；载荷必须合法 JSON，事件名禁 `internal/*`
    前缀；无次序承诺（与 in-process emit 一致）。
- 新增状态码：`STATUS_UNDECLARED(-3)` / `STATUS_INACTIVE(-4)` /
  `STATUS_REENTRANCY(-5)` / `STATUS_HOST_ERROR(-6)` /
  `STATUS_INVALID_PAYLOAD(-7)` / `STATUS_INVALID_EVENT(-8)`。任何非
  OK/CAPACITY 状态可在 out buffer 携带 `{"code","message"}` JSON 详情；
  `STATUS_FAILED` 的详情为目标业务错误透传。
- 重入纪律（D-V3-3 b'）：宿主以 thread-local 激活栈覆盖全部外来入口
  （setup/call/on_event/on_next/teardown）；同线程回环 = 确定性
  `reentrancy_cycle` 错误，永不死锁；合法嵌套链与自有线程调用不受影响。
- 生命周期与线程纪律：vtable 按激活期构造（再激活 = 新 vtable、新 handle，
  与旧者独立）；teardown 返回后的 vtable 调用 = 文档化 UB（不做运行时检测）；
  插件自有线程须在 teardown 返回前终止并 join；宿主全部 vtable 入口
  panic-contained。
- 加载门：`ABI_VERSION` 2 → 3，v2 工件硬拒载（错误消息含两侧版本），须按
  v3 重编译。

迁移（v2 → v3）：

1. `setup` 改收 `*const HostExports`；不需要出站能力时忽略之即可，需要时按值
   复制存入插件状态。
2. 出站 `require` 只能命中 descriptor `inject` 声明集内的服务。注意声明即
   fiber 级依赖：声明而无人提供 ⇒ 插件 fiber 停 `Pending`，所有加载该插件的
   世界都必须提供这些服务。
3. 插件自有线程共享的状态须自行同步（宿主每实例锁不覆盖自有线程）；
   `teardown` 返回前 join 全部自有线程。
4. 对齐 `ABI_VERSION` 重新编译；宿主侧 `dougong-dylib` / `dougong-loader` 随
   0.3.0 同步升级。

验证锚：`cargo test -p dougong-dylib`（E1.1–11 出站矩阵）；
`examples/outbound_demo.rs`（自有线程全链 + 权限拒绝 + 重入环两臂）；
`greeter-plugin` 自带自有线程 require/emit 演示（worker 线程在 setup 内
spawn、teardown 内 join）。

### 既有面零回归

in-process API、五事件派发模式、wasm 面（dougong-wasm / WIT / wasm_demo
零变更）、loader reconcile 语义、descriptor schema 均不变。

## [0.2.0] - 2026-10-21

realm 作用域能力集（D15，破坏性变更）：loader 的 `isolate` 行成为 realm——
成员 provide 落域标签，成员注入域内优先、缺失回退全局；域服务不外泄到默认
标签。同名 seam 可按域共存（每会话能力集）。内核 `ScopeNode` 增域级回退标签；
新增 `Context::isolate_realm`；`isolate` / `isolate_with_label` 按名语义不变。
