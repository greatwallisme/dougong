# Changelog

本仓库的显著变更记录。版本语义：0.x 系列的 minor bump 即可含破坏性变更。
设计裁决全文见 [doc/research/dougong-design.md](doc/research/dougong-design.md)。

## [0.6.0] - 2026-11-05

### ABI v6：dylib 出站 realm 激活（破坏性变更）+ `Context::get` 的 realm 语义（D15 完整化）

dylib 插件可在自有线程上新建 realm、在其中激活子插件工件、调用子激活提供的
服务并单独卸载它们——`ctx.isolate_realm(l).plugin(..)` 的跨 C 边界对应物，
面向"每会话能力集"（fly-rafter 的会话 realm / 子 agent 子 realm）。裁决全文见
D33，需求规格见 `doc/maintenance/realm_requirements.md`（含 §9 实现期修订）。

v5 → v6 diff 清单：`HostExports` 在 `dispatch` 后追加 `realm_spawn`、
`realm_retire`、`realm_require` 三字段；新增常量 `REALM_SPAWN_REPLY_MIN`。
`PluginExports`、descriptor schema、require/emit/grant/dispatch、CPS 协议、
wasm/WIT、loader JSON 不变。

- `realm_spawn(realm, path)`：`realm == 0` 分配新 realm，否则须为本激活此前
  返回的标签；按路径加载并同步启动子激活，回复
  `{"realm","child","state":"active"|"pending"}`；加载失败
  （`realm_spawn_load_failed`，含 `abi_version` 不符）与子激活启动失败均无残留。
  回复缓冲低于 `REALM_SPAWN_REPLY_MIN`（96）在任何副作用之前回
  `STATUS_CAPACITY`。
- `realm_retire(child)`：卸载单个子激活并等其 teardown 收敛；状态码形，无详情缓冲。
- `realm_require(realm, service, ...)`：`require` 的 realm 寻址版，无需 `inject`
  声明；仅放行本激活所 spawn 的子激活提供的服务（授权绑定解析到的提供者激活
  id），其余 = `STATUS_UNDECLARED`；与 `require` 共用同一调用路径，S5 提供者
  隔离与 O7 边界追踪照常生效。
- 姿态与生命周期：`realm_spawn`/`realm_retire` 必须自插件自有线程发起，外来入口
  帧内 = `STATUS_REENTRANCY`；子 fiber 是 spawner fiber 的 effect，spawner
  teardown 先级联卸载全部子激活再跑自身 `teardown` 导出。
- 加载门：`ABI_VERSION` 5 → 6，v5 工件硬拒载（错误消息含两侧版本）。
- 新增示例工件 `realm-partner`；`outbound_demo` 增 realm 臂。

**行为变更（内核）**：`Context::get` 动态分支（fiber 未声明的名字）此前只读全局
label、忽略 realm；现与依赖解析同序（按名隔离 → 最内层 realm → 全局，首个
strict 命中）。realm 内未声明名的 `get` 因此优先取域内提供者；D31 grant 的目标
解析对未在授予者 `inject` 内的目标名同样获得 realm 优先（修正 0.4.0"按授予者
realm 解析"声明与实现的偏差）。无 realm 使用的宿主不受影响。

测试夹具修复：`dispatch_tests` 的监听插件声明 `inject: [svc-dispatcher]`，先于
driver 启动时首次 `join` 在 Pending 返回，派发可能抢在事件桥注册之前（基线约
6% 间歇失败）；夹具改为 driver 就绪后再 `join` 监听插件并断言 Active。

升级清单：

1. 插件对齐 `ABI_VERSION` 重编译（`HostExports` 按值复制时自然带上新字段）。
2. 宿主依赖升 `dougong* = "0.6.0"`；不使用 realm 的插件无需代码变更。

## [0.5.1] - 2026-11-04

### 修复：dylib 拆除与插件自有线程出站 require 的锁序倒置（停摆纪律）

disposer 在实例锁内运行 teardown，而插件按 v3 纪律在 teardown 中 join 自有
worker 线程；该 worker 的出站 require 以同一把实例锁为 caller 串行化点，
排队等待者「获锁后见门翻」的退场条件与 disposer「join 到该线程」互为前提
——0.5.0 中该环无解，首次全量真共载（20 dylib 工件 + 信号关停）以 ≈50%
概率挂死。修复（需求规格全文见 `doc/maintenance/teardown_requirements.md`）：

- disposer 两臂（registered effect 与 inline fallback）：liveness 门翻转
  上移到锁获取**之前**；获取保持普通 `lock().await`（tokio Mutex 公平队列
  —— disposer 之前的每个排队等待者获锁即撞锁内门、以 `provider_gone`
  退场，disposer 的轮次到达时队列必空，有限步收敛，无轮询无睡眠）。
- `DylibInvoke::call` 与事件桥新增**预锁** alive 检查：门翻之后的新 require
  / 监听者在排队前即失败，从源头保证无新等待者。
- 事件桥补上**锁内** alive 检查：0.5.0 桥无任何门，teardown 后获锁的排队
  监听者会进入已释放的 vtable（既有 UB 洞，一并封口）。
- `dispatch` 入口新增早门：门翻后新发起的 dispatch 即刻
  `provider_gone`；在途 dispatch 经链内监听者在门处退场自然收敛。

语义零变更：`provider_gone` 状态码与文案、`STATUS_OK` 路径、reentrancy
纪律、同一实例外来入口不交错的串行化保证逐字节不变；loader JSON、wasm/
WIT、CPS 协议、descriptor schema 不变。新增回归钉测
`teardown_joins_a_requiring_worker_without_deadlock`（插件自有 worker 循环
dispatch、链路进入被拆激活自身事件桥，与 disposer 的 teardown join 竞争
同一把实例锁，限时收敛；require 面因 descriptor 自注入会死锁响应式解析，
无法在不借助 grant 自授权的条件下构造，钉测注释内注明）。已实证钉测对
0.5.0 判别：旧码挂死、新码限时收敛。验证：四关零警告零错误，workspace
241 passed / 0 failed，六 demo exit 0；下游 fly-rafter 18a 真共载复验：
修前关停挂死 ≈50%，修后 boot 5/5 稳定、SIGINT 130 / SIGTERM 0 界内收敛。

## [0.5.0] - 2026-11-02

### ABI v5：dylib 出站同步派发 dispatch（破坏性变更）

dylib 插件可向内核事件总线按五模式（既有 `MODE_*` 词表）同步派发一次事件：
await 型模式阻塞调用线程至链收敛并把终值 JSON 写回 out_buf，`MODE_EMIT`
入队即返（`emit` 保留为规范火忘入口）。裁决全文见 D32；需求来源为
fly-rafter 压缩切片的六个跨插件事件监听（裁决定必须有回程值，事件面
rendezvous 受跨实例等待环检测阻断不可实现）。本版本**推翻 v3 需求文档
§5.5**（出站非 emit 模式结构性排除；依据修正见 D32）。

v4 → v5 diff 清单仅一项：`HostExports` 在 `grant` 后追加 `dispatch` 字段及
配套派发语义。`PluginExports`、descriptor schema（监听仍 = `listen` 静态
声明）、require/emit/grant、CPS 协议、wasm/WIT、loader JSON 不变。

- waterfall fallback = 固定恒等续体：无监听者生效 ⇔ 链终值 == 载荷字节
  （桥回写原始字节，键序归一下字节稳定）；差异化默认值由载荷协议承载。
- 链内监听者失败 = `STATUS_FAILED` + `{"code","class","message"}` 详情
  （请求语义非广播）；serial/bail 传播首个失败、parallel 聚合。
- 姿态门：必须自插件自有线程发起；外来入口帧内调用 = 确定性
  `STATUS_REENTRANCY`（锁死组合检测不可达，保守前置拒绝，D32 B6）。
- 非法 mode 值 = `STATUS_FAILED`（`invalid_dispatch_mode`）；`internal/*`
  事件名与非 JSON 载荷对齐 `emit` 既有拒绝。

## [0.4.0] - 2026-10-28

### ABI v4：dylib 回调能力授予（破坏性变更）

服务拥有者可在运行期把「可 require 我的一个服务」授给特定的已提交目标激活，
无需目标在 descriptor `inject` 中静态声明。裁决全文见 D31；需求来源为
fly-rafter 的通用决策型策略注册面（回调必须有同步返回值，事件 rendezvous
无法替代）。

v3 → v4 diff 清单仅一项：`HostExports` 在 `emit` 后追加 `grant` 字段及配套
授权表语义。`PluginExports`、descriptor schema、require/emit、五种事件模式、
wasm/WIT、loader JSON 和既有内核调用行为均不变。

- `grant(target_service, granted_service)` 只登记、不调用、不写 out buffer；
  任意插件线程并发可调，不获取实例锁，不参与重入栈/等待图。
- 只能授予自己 descriptor `provide` 表内的服务名。目标在授予时按授予者 realm
  解析且必须已提交；目标身份为激活 id，提供者换件不继承。
- 校验：非 UTF-8 = `STATUS_FAILED`（`invalid_grant_name`）；非自有 =
  `STATUS_FAILED`（`grant_not_owned`）；目标未提交 = `STATUS_INACTIVE`
  （`inactive_dependency`）。同三元组重复授予 = `STATUS_OK` 且簿记单条。
- 授权随授予者激活回滚清除，新激活从空集开始；成功授予与批量清理均记
  `outbound_grant` 审计日志。无 revoke、通配、方法粒度、转移或再委托。
- require 判定序：静态 inject 命中不触授权表；静态未中先查授权名索引，无名
  条目保持逐字节 UNDECLARED。有名条目时仍先解析所请求服务，未提交 =
  INACTIVE，已提交但未获授 = UNDECLARED；获授后的调用路径与静态放行完全相同。
- 内核仅新增 `Invoke::activation_id()` 稳定身份助手（默认 `None`），由
  `DylibInvoke` 实现、`BoundaryInvoke` 转发，避免 O7/S5 装饰器使 Arc 身份失真。
- 加载门：`ABI_VERSION` 3 → 4，v3 工件硬拒载（错误消息含两侧版本）。

审查修订（2026-10-28 全项目审查，维护者裁决，发布前并入本版；全文见
D31-amended 与需求文档 §7）：

- 授权绑定解析到的提供者：放行条件 = 目标 realm 解析到的提供者激活 id 属于
  以该调用方为目标的 granter 集——跨 realm 同名提供者与换供窗口不继承授权
  （E1.13）。
- 在途 require 与授予者 teardown 的活性门：授予目标非内核依赖边、不被 drain，
  `DylibInvoke` 获实例锁后先查激活存活门，已 teardown 返回
  `STATUS_FAILED`（Transport 类 `provider_gone`），杜绝悬垂 Handle 的 UAF。
- 无激活 id 的 grant 目标（in-process/wasm Invoker）直接
  `STATUS_FAILED`（`grant_target_unsupported`）；授权表条目不再保留目标 Arc。
- 授权名索引按内核 root 作用域化：内核新增 `Context::root_id()` 助手，跨 root
  的未声明 require 保持逐字节 UNDECLARED（E1.15）；空表时静态未中路径不取
  授权表锁（`GRANTS_LIVE` 原子门）。
- E2 的第二插件为真实 cdylib 工件 `crates/grant-partner`（demo grant 臂经
  `load()` 装载）；测试二进制加 120s 墙钟看门狗（同步 FFI 死锁无法被 tokio
  timeout 抢占）。

迁移（v3 → v4）：

1. 插件对齐 `ABI_VERSION` 重编译；`HostExports` 按值复制时自然带上第 4 字段，
   手工构造 vtable 的宿主/测试表须补 `grant` 指针。
2. 宿主侧 `dougong` / `dougong-abi` / `dougong-dylib` / `dougong-loader` 同步升
   0.4.0；既有 descriptor 与装载文档无需修改。
3. 需要运行期回调的插件由服务拥有者在目标已提交后调用 `grant`；目标换激活或
   授予者再激活后重新授予，不依赖旧授权继承。

验证锚：`cargo test -p dougong-dylib`（ABI v3 回归 + ABI v4 E1.1–16 与审查
钉测：提供者绑定、teardown 活性门、跨 root 隔离、无 id 目标拒绝、空表零锁）；
`examples/outbound_demo.rs`（既有出站/权限/重入臂 + greeter→grant-partner 真
实工件 grant 回调往返）；下游 fly-rafter 的零改第三方策略插件接入验收（E4）。

## [0.3.0] - 2026-10-25

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

### 稳定性轨道（improve 二.1–7）

- **S1 状态机 invariant 检查层**：每个 fiber 生命周期转换（spawn/load/
  unload/fail/dispose/driver 落定）跑对应检查点；release 保 O(1) 字段关系
  校验，debug 加 store/index/registry 交叉一致性；破坏即 panic 并附定位
  快照（内核缺陷，非可恢复错误）。
- **S2 cancel scope**：fiber 拥有的全部任务（监听器调用、插件体）经每激活
  期 `CancelScope` 生成令牌派生；卸载先收割后排干效果，未启动即撤、运行中
  在 yield 点中止；边界工作（桥、wasm 体）tracked-only 永不中途丢弃。
- **S3 关键异步路径超时**：`Context::new_with_timeouts` 注入五路预算
  （apply/listener/drain/cleanup/reap-watchdog），全默认 `None` = 行为
  逐字不变；apply 支持有界重试策略。
- **S4 生命周期护栏**：`dispose` 单胜者幂等（终态旗先于 uid 收取）；
  `restart`/`update` 对已 dispose 的 fiber 返回 `fiber_disposed`，对
  Loading/Unloading 中拒绝。
- **S5 错误分类 + 分级策略**（D20）：每个 `Error` 带构造定类
  （Logic/Timeout/Transport）；`ErrorPolicies` 按类定日志级别、监听器失败
  反应（重启/处置肇事 fiber）、提供方隔离（transport 失败处置提供 fiber）。
- **S6 确定性更新排序**（D21）：同一依赖更新组的 `{变更+通知}` 经
  `(label, name)` 串行段全序化；组间互不阻塞；派发模式观察序仍无承诺。
- **S7 故障注入套件**：12 例落四 crate（并发热更、卸载+注入+事件竞态、
  wasm 陷阱毒化、teardown 失败、drain 超时等）。

### 性能轨道（improve 一.1–7）

- **P1 根状态分片**：reflect store / 依赖索引 / fiber 表按名或 uid 分片
  （16 条带），锁序纪律成文；基准 B1≈2.5×、B2≈2.4×、B3≈1.2×；顺手修复
  勘察发现的 notify 锁拓扑死锁与提交键漂移两缺陷。
- **P2 事件派发队列化**（D22）：fire-and-forget emit 走 per-event 就绪道 +
  固定 worker pool（默认 4）；可靠同步 emit 仍无损无界；有界道/背压/按名
  可丢弃全部 opt-in，默认语义不变。
- **P3 notify 合并**：审计 + 计数对照证实既有机制已饱和需求实质
  （负容量裁决，bench_notify 锚点入库）。
- **P4 无锁活跃集**：固定原子位图镜像替换单锁活跃集，严格解析与转换翻转
  无锁；B2 解析风暴 ≈1.35-1.4×。
- **P5 label 候选零堆分配**：性能中性，保留依据 = 表示简化。
- **P6 事件匹配索引**：负容量裁决（按名索引已内建，匹配份额 ≤3-5%）。
- **P7 插件预算**（D23）：`BudgetConfig` 可按 fiber 限存活 effect 数与
  每激活窗口事件发射数（全默认 `None` = 零成本零改变；内核协议事件豁免）。

### 可观测性轨道（improve 三.1–7）

- **O1 结构化日志**（D24）：内核全部日志线升级为 `LogRecord` 字段表；
  `Logger::log_record` 默认平面降级，存量 sink 零改动；不引 tracing 依赖。
- **O2 运行时 metrics**（D25）：`Context::metrics()` 快照（fiber 态分布、
  常开计数器、opt-in 延迟直方图）。
- **O3 trace id + boundary tag**（D26）：每 fiber 构造期 trace id（dispose
  撤 uid 不撤 trace，全链可 join）；全部记录带 trace 与插件形态 boundary
  tag（dylib/wasm）；`Context::trace_id()` 公开面；转移记录带离开态时长。
- **O4 状态事件体系**（D27）：`internal/fiber.created`、
  `internal/fiber.<state>`、`internal/provider.changed`、
  `internal/effect.cleaned`、`internal/drain.started/finished` 一致载荷
  （service.notified / event.dispatched 映射既有 internal/service 与
  internal/dispatch，不建重复通道）。
- **O5 健康检查**（D28）：`Context::health()` 快照（计数 + failed 明细 +
  recent errors 环 + 长任务 + stale providers + 默认 healthy 判定）；
  HTTP 端点归宿主（`examples/health_demo.rs` 演示端点体）。
- **O6 错误分组与告警**（D29）：八分组 `ErrorCategory` 视图 + 每组
  severity 覆盖 / 告警阈值 / 限噪 rate limit（限噪只丢日志线，环与计数
  照收；空规则 = 行为逐字不变）。
- **O7 边界调用追踪**（D30）：`ContextOptions::boundary_trace` 开启后每次
  动态通道调用出 `boundary_call` 记录（service/method/payload size/
  boundary type/duration/成败 + 提供 fiber 身份）；dylib require 出站记
  `outbound_call` 线；默认关 = 零成本。

### 既有面零回归

in-process API、五事件派发模式、wasm 面（dougong-wasm / WIT / wasm_demo
零变更）、loader reconcile 语义、descriptor schema 均不变。稳定性/性能/
可观测性三轨道全部默认零行为改变（预算全 None、策略表空、追踪关），
既有工作负载逐字不受影响；性能基准与 bench_notify 锚点全程逐字节核对。

## [0.2.0] - 2026-10-21

realm 作用域能力集（D15，破坏性变更）：loader 的 `isolate` 行成为 realm——
成员 provide 落域标签，成员注入域内优先、缺失回退全局；域服务不外泄到默认
标签。同名 seam 可按域共存（每会话能力集）。内核 `ScopeNode` 增域级回退标签；
新增 `Context::isolate_realm`；`isolate` / `isolate_with_label` 按名语义不变。
