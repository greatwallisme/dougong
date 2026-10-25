# dougong（斗拱）实现研究：设计文档 v2

> v1 修订：Q4–Q14 按用户反馈重做——工程决策改为**明确决策+依据**，删尽"二期再说"式回避；只保留真正需要用户拍板的产品判断（R1–R3）。
> 证据链：study-1（论文）/ study-2（TS v4.0.5 @ 687f282e86）/ study-3（13 项目含 commit）/ study-4（wasmtime/wit 技能 + 生态 WIT，标注版本）。

## 0. 已由用户裁决

- **Q1** 运行时 = tokio 1.x
- **Q2** 服务键 = 混合：进程内类型键（编译期安全）+ 跨边界 `service-key{interface, abi-hash}`
- **Q3** 父→子级联 = 完整实现（ctx.plugin 注册为父 fiber 的 effect，论文 Eq.31 语义）

## 1. 内核必留语义（不变，11 项表见 v1 归档；要点）

effect ledger（LIFO 可逆变更）/ provide-withdraw 即 effect 且撤回内嵌 drain / inject+epoch 提供者指纹+ω 提交视图 / 六态生命周期+惯性迁移 / ACTIVE 边沿通知 / isolate+intercept / 事件五模式+internal/* 扩展点 / 期望态 reconcile / 级联 / 事务式激活替换 / capability=声明面、沙箱=执法面。

## 2. 工程决策（我拍板，附依据，可否决）

### D4 迁移引擎 = 每 fiber 独立 driver 任务循环（redoop 式强化）
- 语义要求：同 fiber 迁移严格串行；落地后与最新 target 比对（Thm64 一致性）；target 过期则链式反向迁移（惯性）。
- 决策依据：TS 的 promise 链 inertia 在 Rust 等价物会引入"迁移 Future 被丢弃"故障类（jincdream 专门为此设 future-cancellation 测试）；持有迁移所有权的 driver 任务天然取消安全；desired/loaded/dirty/force 四态机可穷举测试；redoop@c50d4ff 已验证。
- 规范：`DriveState{desired, dirty, force, task}`；join 用 watch 通道；restart=force 全循环；dispose=置 INACTIVE 指纹后 join 收敛。

### D5 dylib 卸载姿态 = teardown + 代码驻留（dlopen 不逆转）
- 依据：disposer 是宿主内存中的闭包，可回调插件代码——映射一旦解除即 UB；Rust 无机制保证 .so 满足 dlclose 安全条件（无 TLS 析构/无存活线程/无泄漏引用，redoop 注释明言放弃）。
- 允许的重载：**禁止同路径重复映射**；**允许新路径加载新版本**（旧版本镜像常驻，成本=重载次数×镜像大小，有界）。
- 真卸载（释放旧镜像）= 明确非目标 → 见 R1 产品判断。

### D6 跨边界服务 = 接口化调用；WIT 单一真源（合并原 Q6+Q7，"bytes vs lease"系伪对立）
- **内核通道必须动态**（`service-key{interface, abi-hash} + method + payload:list<u8>`）：cordis 空间可组合性要求服务晚到/先走/被替换时消费者反应式重解析（Eq.24-26）；静态类型 world 在实例化时绑定导入，**表达不了反应式解析**——故全镜像动态通道是语义必然（cordis_wasm@127e1dd 已验证可行），不是偏好。
- **类型安全不牺牲**：每个服务接口用 WIT 定义；codegen 产出三方包装——wasm guest（wit-bindgen 0.61+）、dylib（wit-bindgen c / 手写 C shim）、进程内 Rust（trait impl）。序列化是 codegen 细节（默认 schema 化二进制；abi-hash 覆盖"接口+编码"）。
- **有状态句柄用 WIT resource**（registration/lease）：宿主持表，borrow/owned 语义由 Component Model 定义（[S2]）。lease 只是 resource 的一种用法，不构成独立选型。

### D7 dougong WIT world 草图（v0，实现时以当版 wasm-tools 校验）
```wit
package dougong:kernel@0.1.0;
interface types {
  record service-key { interface: string, abi-hash: list<u8> }
  record event-key { name: string, abi-hash: list<u8> }
  enum event-mode { emit, parallel, serial, bail, waterfall }
  variant kernel-error { inactive-context, inactive-dependency(service-key),
                         undeclared-dependency(service-key), capability-denied(string),
                         internal(string) }   // 两级解析错误进 ABI（对齐 TS Alg6）
}
interface host {
  log: func(level: string, msg: string);
  call-service: func(key: service-key, method: string, payload: list<u8>)
                -> result<link<u8>, kernel-error>;      // link<u8> 即 list<u8>，示意
  provide-service: func(key: service-key) -> result<registration, kernel-error>;
  register-listener: func(ev: event-key, mode: event-mode) -> result<registration, kernel-error>;
  waterfall-next: func(token: u64, payload: list<u8>) -> result<event-reply, kernel-error>;
  effect-collect: func(label: string) -> result<effect-handle, kernel-error>; // 多步激活可逆收集
}
interface plugin {
  descriptor: func() -> plugin-descriptor;   // {name, version, wit-version, inject, provide, config-schema-hash, capabilities}
  apply: func(ctx: call-context, config: list<u8>) -> result<_, kernel-error>;
  handle-service-call: func(key: service-key, method: string, payload: list<u8>) -> result<list<u8>, kernel-error>;
  handle-event: func(ev: event-key, listener: u64, mode: event-mode, payload: list<u8>, next: option<u64>) -> result<event-reply, kernel-error>;
  run-disposer: func(handle: effect-handle) -> result<_, kernel-error>;
}
world dougong-plugin { import host; export plugin; }
```
- 相对 cordis_wasm 的差异点：effect 句柄进契约（论文 effect 迭代器跨边界）、abi-hash 进 key、waterfall-next 显式化。
- **D7 落定（2026-10-21，Phase 5，以 `wit/dougong-plugin.wit` 为准）**，对 v0 草图的修正：
  - `method: string → u32`：与内核动态通道、dougong-abi 的方法选择子统一为一个约定。
  - `mode` 从 `register-listener` 移到 `dispatch`：cordis 语义里模式是**派发时**属性（TS events.ts 的 emitter 侧），内核监听器回调不感知模式；五模式经 guest 侧 `dispatch(ev, mode, payload)` 全量进契约，waterfall 派发的 fallback 为恒等。
  - `handle-event` 去掉 `mode`/`listener` 语义负担：签名定为 `(ev, listener, payload, next: option<next-token>)`；宿主每次派发分配 next-token，guest 拉 `waterfall-next` 推进剩余链，不拉即否决（D8 不变）。
  - 事件与配置载荷统一 `list<u8>`：事件载荷 = 内核 JSON Value 的序列化（JSON null ⇔ 内核 Null）；`event-reply` 类型取消（就是 bytes）。
  - `effect-handle` 编号 0 保留为插件 teardown；`effect-collect` 从 1 起。`registration` 为宿主持表的 capability 证物，guest 侧 drop 不撤回任何东西——撤回仍是声明式的（fiber dispose 驱动 LIFO）。
  - 可重入约束：wasm 实例是单线程沙箱，实例执行中再调它自己提供的服务返回 `kernel-error::capability-denied`（wasmtime 禁止并发进入同一 store）；跨插件调用环会互等死锁——这与进程内内核对 dylib 的行为一致，属内核级属性，非 wasm 特有。
  - 版本锚：wasmtime 48 / wit-bindgen 0.61（cordis_wasm@127e1dd 实证线，rsproxy 当前稳定版）；guest 直接 `cargo build --target wasm32-wasip2`（wit-bindgen 宏），不依赖 cargo-component。

### D8 事件五模式全量进契约（不延后）
waterfall 跨边界 = 宿主持剩余链 + next-token（cordis_wasm 已建模；nucleus 的"guest 拿不到 handle"难题由此解）。emit/parallel/serial/bail/waterfall 一并规范；沙箱侧并发预算由 fuel/epoch 约束（[S1]）。

### D9 声明式配置与 loader 的真实切分
- **D9 落定（2026-10-21，Phase 6，`crates/dougong-loader`）**：
  - 文档 = 完整期望态（非增量补丁）；`plugins[]` 每槽 `{name, source{kind: dylib|wasm|builtin, path/name}, config, isolate}`；name 为 reconcile 键（唯一非空校验）。
  - reconcile = diff 收敛：insert / config-fingerprint 变化走 `FiberHandle::update`（internal/update waterfall，重启式热更）/ 缺席 retire / 全同 unchanged；isolate 变更 = retire+insert（作用域在 fiber 启动时烘进 ctx，不可迁移——Thm73 保证最终态收敛）。
  - **scope→label 映射在 loader**：`ctx.isolate(name)` 每次 mint 新 label，同名 isolate 不会自动同 realm；loader 自持 `scopes: name→label`（`ctx.fresh_label()` 分配），同文档同 scope 的插件共用一个 label。
  - `builtin` source：基座自带插件（harness 自己的进程内插件）与外部 artifact 走同一套声明式管理（TS cordis 场景即如此）；从 Reconciler 构造时注册的注册表解析。
  - **文件监视不在 loader**：watcher 是事件源而非收敛逻辑；任何触发器（inotify/configmap/REPL）把新文档喂给 `replace + apply` 即复用同一条收敛路径——可分性论证成立，非功能回避。
  - **层序可省的论证（2026-10-21，README 对齐声明追溯）**：dsh 的有序分层承担两件事——(1) 冲突仲裁（多份 patch 顺序折叠，同 id 行最后写入者胜，必须定义优先级）；(2) 启动次序（挂载即启动，依赖未就绪需排在提供者之后）。dougong 中这两件事分别被消解：(1) 文档是完整期望态，变更 = `replace` 整份替换，最新文档即唯一事实源；歧义（重复槽名 / 同名服务同标签重复 provide）一律硬拒而非按序仲裁；(2) 就绪次序由 Pending 语义吸收——所有 fiber 一次性启动，消费者依赖未就绪即挂起、提供者 commit 后自动收敛，文档行序与收敛无关（有测试钉住）。合流性（Thm73）保证结果只依赖最终文档：即便未来引入分层/patch 语法，也只是 `replace` 之前的文档预处理，内核语义不变——"不需要层序"是被合流性消解的，不是未实现。代价如实：dsh 分层的"预定义栈复用 + 局部小补丁"人体工学（profile 堆 bundle）由调用方在 `replace` 前自行拼装文档承担，属格式层取舍，不影响内核。
  - fingerprint：serde_json Map 为有序结构，`to_string` 即规范序（键序不敏感，有单测）。
- 语义不可分部分 = 内核意图面：insert(plugin, config, scope) / update-config / retire / isolate 声明——已含于内核 API（ctx.plugin/update/dispose/isolate），随内核一并交付。
- 可分部分 = 文件格式层（yaml/json、include/patch、文件监视）= 独立 crate **dougong-loader**，与内核同一代码库一并交付。可分性论证：Thm73 合流性保证 loader 正确性只依赖最终配置，内核不因 loader 缺位而改契约。
- llmharness 若先行需要，loader 与内核并行开发亦无耦合。

### D10 热更新粒度 = 插件粒度 + 配置热更（模块级 HMR 从需求中删除）
- TS 的 ESM 模块图 HMR 在 Rust 无对应物——不是延后，是**需求修正**。
- 规范：wasm = 新组件实例化 + 旧实例 drain（事务式，替换天然安全 [S1]）；dylib = 新路径 load + 旧 teardown（D5）；配置 = update() 经 internal/update waterfall（可否决，TS 语义）。

### D11 能力/拦截的内核范围（一次做全契约，策略库属应用层）
- 内核保证：未声明不可访问（两级解析错误）；每 key 拦截元数据 monoid + 访问期 waterfall（TS internal/get-set 对应）；isolate realm 路由（Eq.27-30）。
- 衰减策略（如 fs{read:/repo}）= provider 消费该通道实现；**执法边界 = wasm 沙箱**（fuel/epoch/StoreLimits/最小 WASI 能力，[S1]）；dylib = 可信同权代码（仅 ABI 门）。

### D12 通知 = 反向依赖索引（一等实现，非 stub）
- commit ω 时登记 consumer→provider 边；`relied(n)` O(1)（drain 协议核心）；通知 O(consumers)；提供者 ACTIVE 边沿广播照旧。
- 依据：dougong 定位"复杂业务系统基座"，fiber 规模千级；TS/redoop 线性扫描是该规模下的真实瓶颈；索引簿记只在 commit/refresh/dispose 三个点发生，成本低。线性扫描保留为 debug 断言对账路径。

### D13 wasm async 路线 = 同步 WIT + wasmtime async 传输
cordis_wasm@127e1dd 路线（wasmtime 48 实证）；WIT 级 `async func`（nucleus 的 WASI 0.3 路线）在钉定 wasmtime 版本时复核——若当版稳定则平移，契约不变。

### D14 dylib 事件全量接通 = ABI v2 CPS（DEFERRED / on_next 拆分拉链）
2026-10-21，用户裁决（方案 b，"不接受分期和延后"）。评审实证 v1 缺陷：`on_event` 导出从未被宿主调度（描述符无 listen 声明通道），且同步 `NextFn` 原理上不可行——内核续链是 `BoxFuture`，同步 C 函数无法 await，跨线程 block_on 会破坏 dylib 边界"导出仅在 fiber driver 上调用"的安全论证。修复 = `ABI_VERSION` 1→2：`on_event` 以 `STATUS_DEFERRED` 声明拉链（DEFERRED 期输出为续链 payload，空 = 原样续传），宿主解析后续后经新增 `on_next` 导出回注（`chain_status` = OK/FAILED）；`NextFn`/`next_ctx` 删除；描述符新增 `listen`（事件名 → u32 id）；宿主新增**每实例锁**串行全部跨界调用——修正 v1 的错误安全论证（消费者 fiber 各持 task，内核并不排序跨 fiber 调用）。内核配套：`DispatchMode`/`DispatchListener`/`Context::on_dispatch`（模式于派发时下传桥接；`internal/*` 禁桥）。旧 v1 .so 由加载门硬拒，须按 v2 重编译（当前仅 greeter 样例，未发布故破坏成本最低）。

### D15 realm 作用域 = 成员服务域内命名空间 + 全局回退
2026-10-21，实现 isolate realm 的"每会话不同能力集"（dsh 对齐）时发现：loader 的 `isolate` 只把成员行归到同一标签，不给成员的**同名服务**按域分命名空间——两个域各提供一个同名 seam 会 `duplicate_provide` 碰撞。D15 语义：`ScopeNode` 增加域级回退标签（`realm_child`）；解析候选取「显式按名隔离（严格，单个）→ 最内层域标签 → 全局默认」；provide 落点取首个候选。效果：域成员的同名服务互不碰撞且域内优先；域未提供之名回退全局；全局消费者看不到域内服务。loader 的 `isolate` 字段即声明 realm（`Context::isolate_realm`）。`Context::isolate`/`isolate_with_label` 的按名语义保持不变。配套测试 `realm_scope_namespaces_services_with_global_fallback`（kernel_semantics）。

### D16 dylib 出站能力 = ABI v3 HostExports vtable（setup 携带 + 同线程重入环检测）
2026-10-23，用户四项裁决（需求文档 doc/maintenance/function_requirements.md §4 的 D-V3-1/3/4/5，均采纳推荐项；§5.8 同日显式推翻）：

- **D-V3-1 vtable 传递 = a) setup 携带**：`setup(host: *const HostExports) -> *mut c_void`，`HostExports { host_handle, require, emit }` 在 setup 期间按值复制。理由：vtable 先于 setup 首指令可用，setup 期 require（G5）天然满足；ABI 版本本已必 bump，无兼容包袱；失败模式最少。
- **D-V3-3 重入策略 = b') 同线程环检测**：thread-local 激活栈覆盖全部外来入口（setup/call/on_event/on_next/teardown，guard 不跨 await——任务迁移会破坏线程配对）；环 = 确定性 `reentrancy_cycle` 错误（`STATUS_REENTRANCY`），永不死锁。合法 service→service 链（call 处理器内 require 其他服务）放行；插件自有线程空栈出发，永不入环。否决项：a) 全禁令会禁掉 fr 侧审批→session 落账的合法链（不可用）；c) 嵌套派发独立执行体帧跳跃 + 跨执行体锁等待，复杂度最高。
- **D-V3-4 require 执行器 = 实现期基准**：基线 = `rt.enter()` + 调用线程本地自写 parker `block_on`——不依赖调用线程恰在 tokio 内（B1 定性满足）、不消耗 runtime worker、current_thread 兼容、对 future 无 Send 要求、重入栈在该路径上精确。驱动器收敛为 dougong-dylib 内部单个函数，缝后可换（备选：spawn-to-runtime + 调用线程 park oneshot）。
- **D-V3-5 错误模型 = 状态码 + 详情载荷**：新增 `STATUS_UNDECLARED(-3)`/`INACTIVE(-4)`/`REENTRANCY(-5)`/`HOST_ERROR(-6)`/`INVALID_PAYLOAD(-7)`/`INVALID_EVENT(-8)`；任何非 OK/CAPACITY 状态可在 out buffer 携带 `{"code","message"}` JSON 详情（D20 起增 `"class"` 字段，additive），`STATUS_FAILED` 的详情 = 目标业务错误透传。与内核两级能力检查（undeclared vs inactive，TS Alg. 6）同名对齐。
- **§5.8 推翻（附带裁决）**：事件派发队列化（bounded 队列 + worker pool + per-event-type 并发上限 + 背压 + 按事件名 opt-in 可丢弃）全量入 v0.3.0 范围；默认语义不变（不丢弃、五次序承诺照旧）。
- 生命周期与纪律：vtable 按激活期构造（A3），旧激活回滚后其 vtable 死亡，其后调用 = 文档化 UB（不检测）；插件自有线程须在本次 teardown 返回前终止并 join（A6，成文不强制）；宿主全部 vtable 入口 catch_unwind（A4，panic 不跨界）；emit 不取发射方实例锁（C6 恒合法）；teardown 后在途派发不撤销（C7）。
- 验证锚：E1.1–11 矩阵（`dougong-dylib::outbound_tests`）+ `outbound_demo` 两臂；内核本体零改动（桥全走既有公共面：`ctx.get::<Invoker>` / `ctx.emit` / `Error::new`）；dougong-loader 经 `dougong_dylib::load` 同源复用（D3）。语义差异落附录 A 等价表。

### D17 fiber 级 cancel scope = 代际取消域 + 边界调用只跟踪不中止（S2）

2026-10-24，improve 二.2 落地（设计定稿见 `.planning/2026-10-23-v0-3-0-dylib-outbound-and-runtime-harden/findings.md` §11）：

- **代际 scope**：每 fiber 持 `CancelScope` 代际（`Fiber::task_scope`，`cancel.rs`）；load 铸新代，unload 与 load 回滚臂先 `cancel_and_reap` 再 drain effects ⇒ 在途 guest 调用绝不越过 teardown（dylib「teardown 后调用 = UB」窗口封闭）；`dispose` 先 cancel 解挂在途 body（修复「body 挂起 ⇒ dispose 永久悬挂」这一现行泄漏），reap 留给驱动路径（dispose 可被本 fiber 的 listener 任务调用，自 reap 即自死锁）。
- **姿态二分**：in-process 监听器与默认插件 body = yield 点中止（Rust async 纪律保证）；boundary bridge 与 wasm body = 只跟踪不中止（wasmtime 48 `call_concurrent` 文档明示 drop 不取消在途执行、`call` 对 in-call 递归 panic——store 排他模型不可弃）。`Plugin::body_abort_safe()` 默认 true，仅 WasmPlugin 覆写 false；wasm 挂起由 fuel 预算兜界。
- **监听方视角**：卸载 ⇒ 快照后未启的调用确定性跳过；parallel 聚合将「在途被撤」视为监听者消失（absent）而非失败；serial/bail/waterfall 内联链遇死监听者跳过，waterfall 中死监听者不得 veto。
- **C7 不变**：发射方派发任务独立于发射方寿命，emit 外层任务不跟踪。
- **effect disposer 不挂 token**：中止 disposer 会破坏 LIFO 完备性（剩余 disposer 泄漏）；绑定形态 = 时序（reap 先于 drain）；disposer 超时归 S3。wasm 宿主 epoch ticker 改随 WasmHost drop 中止（同旨附修）。
- **常驻化**：S1 invariant 层 post_unload / post_fail_load / post_dispose 增 release 级断言「cancelled ⇒ task_count == 0」——「卸载后任务计数归零」从测试断言升级为状态机不变量。
- 验证锚：`cancel.rs` 单元 9 例 + invariant 注入 1 例 + `tests/cancel_scope.rs` 7 例（dispose 解挂死 body / 在途 listener 中止 / restart 换代 / parallel 跳过 / serial 死跳过 / waterfall 无 veto / stop 全收口）。

### D18 关键异步路径超时 = TimeoutConfig 五预算 + 按 future 类别的诚实策略（S3）

2026-10-24，improve 二.3 落地（设计定稿见 `.planning/2026-10-23-v0-3-0-dylib-outbound-and-runtime-harden/findings.md` §12）：

- **配置面**：`TimeoutConfig { apply, listener, drain, cleanup, reap_watchdog }` 全 `Option`，默认全 `None`（opt-in）——既有面零回归的硬保证；机制在内核、策略值由宿主经 `Context::new_with_timeouts` 接线，与 OBS 裁决（库级 API + 宿主接线）同构。默认翻转是单行变更，config 即缝。
- **策略按 future 类别分派，不假装抢占 FFI**：abort-safe（in-process body / plain listener / plain disposer）超时 = yield 点丢弃（S2 同粒度）；tracked（bridge 调用 / wasm body / 边界 teardown）超时 = 停止等待 + 错误入账 + 外方调用在监听方 scope 内继续存留，unload reap 仍收口（wasmtime 只禁 drop 在途 `call_async`，暂停轮询合法）。dylib 同步 FFI 死循环任何任务机制都不可杀——由 reap 看门狗周期告警暴露，永不绕过 reap 进 teardown（UB 铁律）。
- **apply**：`ApplyTimeout { budget, policy: Fail | Retry(u32) }`。超时 ⇒ cancel scope + await 落定 + 事务性回滚（不 park）⇒ drive_loop 裁决：重试余量内直接重载（事务性保证重试安全），耗尽 ⇒ `apply_timeout` 错误 park Failed；并发改期（dispose/换 epoch）⇒ 不重试不 park（防 clobber 新 desired）。重试预算随 epoch 变更/加载成功/显式 restart 复位。
- **listener**：`parallel` 超时计入聚合失败；`serial`/`bail`/`waterfall` 传播 `listener_timeout`；`emit` 记日志。bridge 在 serial/bail/waterfall 的内联路径改为「临时 spawn_tracked 进监听方 scope + await 侧超时」（到期 detach），plain 仍内联 timeout 丢弃。逐 listener 预算，emitter 最坏等 N×budget（成文）。`internal/config` 整链同套 listener 预算（internal 拒桥 ⇒ 整链丢弃安全）。
- **notify** 无独立异步面：同步函数；其暴露面 = internal/service 监听器（listener 预算）+ 收敛等待（drain 预算）+ 调用方 join（既有 join_with_timeout）。
- **drain**：`DrainTimeout { budget, policy: LogAndContinue | CancelConsumer }`，逐 consumer 界 join；CancelConsumer 即文档的 kill-task（撤 consumer scope，abort-safe 卡死落 Failed）。**Thm 63 的撤回等待在 drain 预算下弱化为有界**——这正是该策略的存在意义；S1 invariant 的「dispose/unload 后 provider_consumers[自身] 必空」断言相应改为「drain 预算配置时豁免该臂」（卡死 consumer 的陈旧边在其收敛/dispose 时自清，uid 单调不复用，无后续污染）。
- **cleanup**：disposer 增 criticality。Critical（provide/accessor 撤回、listener 摘除、registry 级联）永不孤化——其内部等待由各自预算界或同步即返；Orphanable（插件 ctx.effect、边界 teardown）超时 ⇒ 弹入无跟踪任务继续跑（不 abort：wasm teardown 禁 drop、dylib 同步 FFI 不可 preempt）+ 记 `cleanup_timeout` + 继续 LIFO。契约成文：cleanup 预算下插件 teardown 可与下次激活重叠。
- **reap_watchdog**：只告警不抢占；周期 Error 级日志（fiber 名/uid/滞留任务数）。S2 结构洞察：reap 挂死 ⇔ tracked 调用挂死，故治本在各 tracked 面的超时，看门狗兜不可杀类别的可观测性。
- 错误码 `apply_timeout` / `listener_timeout` 入 Error 稳定码；drain/cleanup/reap 产结构化日志行（含码词），S5 在此三分之上建分级。
- 验证锚：`tests/timeouts.rs` 12 例（apply 超时/重试成功/重试耗尽、parallel plain 入聚合、parallel bridge detach 存活、serial 传播、waterfall 传播且 fallback 未触、emit 记日志、drain 两臂、cleanup 孤化续 LIFO、reap 看门狗告警后收口）；默认零配置下既有全套 17 套件回归即零回归证明。

### D19 生命周期 API 幂等护栏 = 咽喉守卫 + 单 winner dispose + 入口状态拒绝（S4）

2026-10-24，improve 二.4 落地（设计定稿见 `.planning/2026-10-23-v0-3-0-dylib-outbound-and-runtime-harden/findings.md` §13）：

- **根因洞察**：一切激活请求（`restart`/`update` 的 refresh、provider 通知的 refresh）最终汇于 `request_epoch` 单一咽喉，而 driver 状态机没有终态概念——护栏加在咽喉而非逐调用点。实证缺口三处：并发双 dispose 双 winner（check-then-act 跨两次取锁）⇒ `internal/plugin` disposed 事件重复发射等污染；dispose 的 uid 认领先于 `remove_fiber`，窗口内 notify 收集到的 refresh 可把活跃 epoch 打回 ⇒ **已 dispose fiber 真被重新激活**；update 在 Loading 时返回 Ok 但在途激活早已 clone 旧配置（误导调用方）。
- **咽喉守卫**：`DriveState` 增终态标志 `disposed`；dispose 同步头**先**置位（driver 锁内）**后**原子 `uid.take()` 认领（顺序不可颠倒，否则多线程下漏出「flag 未置位 + uid 已 None」窗口）。`request_epoch` 在 `disposed && desired != EPOCH_INACTIVE` 时直接拒——已 dispose fiber 永不再次收敛到活跃 epoch；INACTIVE 方向保持合法（dispose 自身经此收敛）。
- **dispose 单 winner 幂等**：winner 独占 teardown（事件单次、remove_fiber 单次、post_dispose 检查点单次）；loser 不早退而是 request INACTIVE + join 收敛后返回——「多次调用返回同一状态」解释为**所有调用方返回时 fiber 均已收敛 Disposed**。loser join 无死锁：dispose 同步头保持无 await（S2「从本 fiber 监听任务内 dispose」语义不回归），driver 任务不属任何 scope 恒推进至落地。
- **入口显式错误**（稳定码入 Error）：restart/update on disposed ⇒ `fiber_disposed(action)`（替代语义含糊的 `inactive_effect`，需求「error with explicit reason」）；update on Unloading ⇒ `fiber_unloading`（需求字面 reject）；update on Loading ⇒ `fiber_loading`（超字面扩展，依据：在途激活已 clone 配置，Ok 返回即谎言）。入口守卫是进入时刻的点读；waterfall 期间的流转由 driver 串行化 + 咽喉守卫兜底收敛（tail 的 restart 自带 disposed 检查）。
- **Pending/Failed 的 update stash 臂保留**（loader 预配置通路）：清 error + 写 raw_config + refresh；原 `request_epoch(EPOCH_INACTIVE)` 在守卫后证明恒空转（parked fiber desired 已是 INACTIVE）⇒ 删除。零 API 签名变更；边界宿主透传安全（dylib 仅特判 reentrancy_cycle，其余码入详情载荷泛传；wasm 不枚举码）。
- 验证锚：`tests/lifecycle_guards.rs` 6 例（顺序双 dispose 单 teardown / 并发 8× dispose 单 winner + disposed 事件恰一次 / restart+update on disposed 显式错 / update on Unloading（provider 撤回 + 受控门 disposer 确定性构造）/ update on Loading（受控门 body））+ fiber.rs 内联单测（dispose 后直接 refresh 不复活：driver 快照恒 INACTIVE + body 计数不变）；既有全套回归零改动通过 = 无既有流依赖 Loading/Unloading 期 update。

### D20 错误三分 + 分级策略 = Error.class 字段 + ErrorPolicies 政策表（S5）

2026-10-24，improve 二.5 落地（设计定稿见 `.planning/2026-10-23-v0-3-0-dylib-outbound-and-runtime-harden/findings.md` §14）：

- **三分载体**：`Error` 增 `pub class: ErrorClass`（`Logic` / `Timeout` / `Transport`，声明序即严重度 Ord ⇒ `aggregate` 取成员最重）。构造器设定：`new`/`msg` = Logic，`Error::transport` = Transport，三个 timeout 构造器 = Timeout。依据：码派生分类对 `internal` 桶结构性失效；显式字段编译期强制每个构造点做分类决定。
- **码面整顿（「明确报错类型」落地）**：msg 桶细分——内核 `plugin_panicked` / `plugin_cancelled` / `listener_panicked` / `join_timeout`；dylib `ffi_status`（非约状态 + 容量溢出 + DEFERRED 契约违背）/ `plugin_load`（dlopen / 描述符 / ABI 门）/ `payload_codec` / `setup_failed` / `listener_failed`；wasm `wasm_trap` / `wasm_engine` / `payload_codec`。`service_error` 保持 Logic（传输成功、业务失败——类描述的是故障面而非方向）。
- **政策面（OBS 裁决同构）**：`ErrorPolicies { logic, timeout, transport: ClassPolicy, provider_quarantine }`；`ClassPolicy { log_level, on_listener_failure: LogOnly | RestartFiber | DisposeFiber }`；`ProviderQuarantine::Disabled | DisposeOnTransportError`。构造期固定于 RootState，经 `ContextOptions` + `Context::new_with_options` 注入（`new`/`new_with_timeouts` 委托之，签名不变）。**默认 = 全 Error 级 + LogOnly + Disabled ⇒ 零行为变化**（既有全套零改动通过即实证）。
- **四旋钮 → 诚实缝映射**：log level = 全部内核错误日志点（fail_load / fire-and-forget / cleanup / reap 看门狗 / drain / apply 落定 panic）改道政策助手按类取级。crash = listener 失败在**全部五模式的结算点**对肇事 fiber 施加反应（detached 执行，不占派发延迟、不与 drain 成环；uid 0 root fiber 永不为目标）；传播语义不动（五模式契约）——反应是运行时对肇事者的带外动作；body 失败的 transactional Failed 是固定语义非旋钮。retry = 仅 apply 有界重试（S3 已落地，即 Timeout 类重试旋钮）；listener / transport 自动重试**不提供**——投递次数是五模式契约语义，内核无幂等信号，重试归发射方业务层。isolate = quarantine 在 `Context::get` 的 Invoker 解析处包 `QuarantineInvoke`（`Weak<RootState>` 防循环引用），Transport 类调用失败 ⇒ detached dispose 肇事 provider fiber（terminal 无自动循环；loader reconcile 可显式重载）；in-process impl 永不产 Transport ⇒ 对其零影响。
- **边界映射显式化**：dylib 出站详情 JSON 增 `"class"` 字段（additive，状态码集不变，不触 ABI 面）；wasm 的 WIT `kernel-error` 变体集不变（契约变更属 D7 流程，本项不动）——`internal` 串内 code 现已显式（`"wasm_trap: …"`），类型可恢复。
- **「fire-and-forget 吞错误」痛点闭环**：盘点实证 bridge + 预算臂在预算内失败时既不记录也不传播（原代码只在超时分支记录）；现统一按类分级记录 + 政策反应，且一次调用至多反应一次（桥超时先反应，迟到的完成错误只记录）。
- **与 O6 的分界**：S5 三类 = 运行时政策维度；O6（Phase 4，IR 三.6）八类告警 = 可观测性维度，将在本分类日志助手上构建，互不吞并。
- 验证锚：`tests/error_policy.rs` 11 例（构造器分类 / 三类 log level / LogOnly 默认 / RestartFiber / DisposeFiber / parallel 聚合与反应并存 / serial 传播与反应 / quarantine 三臂（Transport⇒dispose、Logic⇒不动、Disabled⇒不动）/ root fiber 豁免）+ dylib `call_statuses_map_to_classes` 与 `outbound_detail_carries_the_class_field`（另有两例既有测试扩类断言）+ wasm 4 例（两级区分保持、Internal 串码可恢复、trap/reentry 分类）；默认零配置回归 = 既有全套。

### D21 依赖更新确定性排序 = 按 service group 串行段（S6）

2026-10-24，improve 二.6 落地（设计定稿见 `.planning/2026-10-23-v0-3-0-dylib-outbound-and-runtime-harden/findings.md` §15）：

- **实证缺口**：provide/withdraw 的「reflect 槽位变更」与「notify（candidate 重算 + refresh + `internal/service` 发射）」是两段分离操作，段间可被同槽位的对立变更插入——同槽并发「替换」时撤销事件可整体丢失（事件流 `[true,true]` 无 `false`）或与提交序倒置，收敛路径（先 Pending 再激活 vs 跳过 Pending 直接换绑）由线程调度器掷骰。TS 参照（单线程事件循环）下该交错本不可达——S6 把参照语义还原回多线程 runtime。
- **机制**：`RootState::ordering`（新模块 `ordering.rs`，crate 私有，零公共 API）= 按 `(label, name)` 键控的串行段表。四处段落点全部同步无 await：provide/accessor 的 apply 臂 `{insert + commit_self + (Active ⇒ notify)}` 与撤回 disposer 同步前缀 `{remove + notify}`；consumer 的 **drain 等待留在段外**（Thm 63 等待不持键——卡死 consumer 不得堵死同组后续更新；无缝换绑路径保留）。键以 provide 落槽的 `primary_label` 同源计算 ⇒ key↔slot 精确对应；键表常驻形态与 reflect/events 的按名驻留同型。
- **机制选型诚实说明**：段体无 await ⇒ keyed parking_lot mutex 即充分；真并发提交者的相对顺序本就由调度器定义，任务队列不改变该事实（承诺 = 同组**全序且连贯**：无撕裂、无丢失撤销事件、事件生成序 == 提交序，而非预定顺序）。coalescing 队列留作 P3 缝（语义不变可换）。
- **不包段的面（附依据）**：加载成功批通知（fiber.rs:976）——槽位所有权 ⇒ 同键写者唯 owner fiber 自身，driver 已串行；registry 初始探针 / `Context::set` / 一切读路径——只读或本 fiber 轴。同 fiber 并发 `update()`（config）交错 = **范围外**（非依赖/事件刷新；S4 已钉入口边界；loader reconcile 串行调用；记录 findings §15 N2 备查）。
- **锁序**：update-key → reflect/index/fibers/candidates/driver，反向路径不存在（读路径不取键；段内 emit 只入队、不在段内执行 listener ⇒ 无 re-entry）⇒ 无环。
- **保证表**：G1 同组全序连贯（无半态/无丢失事件/生成序==提交序）；G2 同 fiber 收敛全序（既有 D4 机制，风暴测试钉死）；G3 并发负载重复运行终态逐字节相同（验收锚）；G4 异组互不阻塞（单测直证）；G5 五派发模式 / C5 无次序承诺 / C7 / S1–S5 语义零改动，always-on 无旋钮（确定性是机制不是策略）。事件**观察**序仍属 C5/C7 契约面（spawn 派发，执行序不承诺）——承诺止于生成序，载荷在段内读取（与提交态一致）。
- 验证锚：`ordering.rs` 单测 3 例（同键零重叠 / 异键不互堵 / 同键锁复用）+ `tests/ordering.rs` 4 例（同槽替换风暴 30 轮逐轮精确终态（multi_thread 4 线程）/ 4 service × 3 consumer 替换风暴 10 轮不动点逐字节精确 / 同 fiber 双 service 并发替换 20 轮无一新一陈 / current_thread 生成序==观察序交替断言）；既有全套零改动通过 = always-on 机制零回归实证。

### D22 事件派发 = 固定 worker + per-event ready lanes（P2）

2026-10-24，improve 一.2 落地（勘察与基准见 `.planning/2026-10-23-v0-3-0-dylib-outbound-and-runtime-harden/findings.md` §18）：

- **任务扩散治理**：emit/emit_internal 不再为每次发射/每个 listener 直接 `tokio::spawn`。listener invocation 进入按 event name 分 lane 的队列，由 construction-fixed worker pool（默认 4）执行；同一 lane 仅在 `active < max_concurrency` 时进入 ready-name FIFO，worker 每次取一个就绪 lane 的一个 job。热点事件达到 per-event cap 后不持 worker 等 semaphore，异事件名不被全局队头阻塞。
- **有界与背压的诚实边界**：现有 `emit(&self, ...) -> ()` 是同步无返回 API，队列满时无法同时满足不阻塞、不丢弃、有界；`blocking_send` 可堵死 current_thread runtime，overflow spawn 又重建无界任务。故默认可靠同步 emit/emit_internal 走**无界可靠 lane**（语义与 C7 不变），有界背压由新增 `Context::emit_with_policy(...).await` 提供，`OverflowPolicy::Drop` 才允许满队丢弃。不得把隐式 drop 或阻塞 runtime 当作默认兼容。
- **配置面**：`ContextOptions.events: EventQueueConfig { workers, queue_capacity, policies }`；`queue_capacity = None` 默认无界，`Some` 限定有界 lane 的**待执行 listener invocation 数**（不含正在执行者）；`EventPolicy { max_concurrency, overflow }` 按 event name 精确匹配，cap 默认仅受 worker 数限制，overflow 默认 Backpressure。`emit_with_policy` 在 Backpressure 下异步等空位，在 Drop 下立即返回 `Dropped`；返回 `Queued` 表示所有匹配 listener job 已接受，`Dropped` 表示至少一个被满队拒绝。
- **S2/S3/S5 语义保持**：`CancelScope::run/run_tracked` 把既有准入计数、取消广播与 reap 泛化到 worker 上 scoped execute；plain listener 在 yield 点取消，bridge tracked-only 至完成，listener budget 与 S5 失败反应仍在原 `run_fire_and_forget`  seam。parallel/serial/bail/waterfall 不排队、不改传播；C5 不承诺观察次序，C7 由 root-owned worker 保发射方独立。
- **生命周期与 panic**：queued job 只持 `Weak<RootState>` + scope/fiber 弱键，消费时升级重建 Context；RootState 释放时关闭队列并唤醒等待者，避免 root→queue→job→root 强环。listener panic 在 worker 的 poll 边界收容，lane active 计数仍释放，后续 job 与其他 listener 继续。
- 验证锚：`tests/event_queue.rs` 7 例（固定 worker 上界 / 异事件不受 per-event cap 队头阻塞 / capacity=1 背压零丢失 / Drop 满队才丢 / panic 后 worker 续跑 / 出队前 dispose 跳过 listener / 多生产者风暴投递与 cap 精确）+ 既有全套（S1–S7、五模式、dylib/wasm 边界）回归。`tests/bench_events.rs` 同机两跑：改造前 wall 13/13ms、max_active 2952/3635；改造后 wall 11/11ms、max_active 4/4，16000 次投递均精确。

### D23 插件预算 = 内核收口点计量，计算计量归沙箱（P7）

2026-10-25，improve 一.7 落地（勘察与设计见 findings §23）：

- **需求四项的覆盖映射**：最大执行时间 = S3 apply/listener/cleanup/drain 预算
  （D18，已覆盖不重建）；只读/只写/调用计量排除——wasm 侧 fuel/epoch/StoreLimits
  已全量（D5/D11），dylib = trusted（R1）、in-process 同地址空间无强制点；内核
  只计量经过内核收口点的两类资源：effect 数与事件发射数。
- **max_effects**（`BudgetConfig::max_effects`）：per-fiber 存活 effect 数上限，
  收口在 `register_effect`（provide/accessor/on/once/on_bridge/用户 teardown 全经
  此点；listener 注册本身即 Critical effect）。计量存活槽位而非 `len`（dispose
  后留 `None` 洞）。breach = `budget_exceeded`（Logic 类）：激活期走既有 fail_load
  回滚，运行期返回调用方。锁内检查 = 严格无超发。kernel 清理只 run 不 register，
  cap 不会打断 teardown。
- **max_event_emits**（`BudgetConfig::max_event_emits`）：per-fiber 每激活窗口
  （`load_once` 每次 body 运行前重置，确定性锚点，拒绝计时器 rate-limit）发射数
  上限。enforcement 在插件面 `emit`/`emit_with_policy` 入口；`emit_internal`
  （internal/service、internal/status 等内核协议）豁免——否则插件预算可切断
  withdrawal 协议的事件传播（`kernel_withdrawal_emissions_exempt_from_budget`
  端到端钉死）。breach = 丢弃该次发射 + `ErrorPolicies.log`（Logic）每窗口仅
  首次（AtomicBool 防日志风暴）；`emit_with_policy` 报告既有 `Dropped` 变体
  （不扩枚举，日志区分原因）。并发准许 `fetch_add` 后 `prev < cap` → 有界软超发
  ≤ 并发数-1（与 max_effects 的严格上限各自成文）。
- **配置面与默认**：`budget.rs::BudgetConfig` 经 `ContextOptions.budgets` 注入，
  全 None 默认 = 零行为变化且零原子操作（Option 读门控）；construction-fixed 同
  S3/S5。root ctx（无 fiber）不计量；dylib/wasm 出站 emit 自动按插件 fiber 计量，
  无 ABI/WIT 改动。
- 验证锚：`tests/budgets.rs` 7 例（cap fail_load + 回滚无残留 / 精确边界 /
  listener 覆盖 / emit 窗口丢弃 + 单条日志 / restart 重置窗口 /
  emit_with_policy 报 Dropped / 内核豁免端到端）+ 既有全套回归 + bench_notify
  锚点逐字节不变（b3=762）。

### D24 结构化日志 = 记录面升级，logger 仍是可替换服务（O1）

improve 三.1 要"结构化日志，不要只用 println"。两条路线：引入 `tracing` 依赖，或升级
内核自有日志面。**裁决：升级自有日志面，不引依赖**（2026-10-25）。

- logger 在 cordis 语义里是一个可替换/可隔离的服务（TS `LoggerService`）；内核引
  tracing 会把 sink 决定权从宿主收走，与组合语义冲突。宿主要 tracing，覆写
  `Logger::log_record` 一个方法桥接即可（Phase 0 裁决 OBS = 库级 API + 宿主接线的实例）。
- `LogRecord` 十字段全表：必有 face（level/target/event 种类/message/timestamp）+
  可选上下文（class/fiber/uid/realm/service/event_name/mode/from_state/to_state/
  cause/duration）。`Logger::log_record` 默认实现平面降级为 `"{event}: {message}
  (k=v …)"`——既有 log-only sink（全部存量测试、StderrLogger）零改动，needle 面保留。
- 全部 10 个内核日志点升级为结构化记录；错误记录统一经 `ErrorPolicies::emit`
  （分级 level + 盖 class 后走 log_record），S5 单源分级不被绕过。
- 状态转移：`cache_state` 返回旧镜像值（swap），6 个 cache+publish 对经
  `publish_transition(from)` 落 from→to 的 Info 日志（生命周期线非错误类，不进 S5
  分级表），再发既有 `internal/status`。转移日志放在 publish_transition 而非
  cache_state：cache_state 无锁，不在其中新增 store 条带锁获取（锁序纪律）。
- 隔离记录（provider_quarantine）从"root fiber 的 logger"改为"provider fiber 自身
  realm 的 logger"——与其余日志点的隔离语义一致；fiber 是局部 Arc，无存活期问题。
  存量行为在无隔离场景逐字节相同（两路都解析到同一种子 stderr logger）。
- 日志率控（限噪）不在 O1 范围，归 O6。
- 验证：tests/logging.rs（10 用例：字段表/平面降级/转移序列/quarantine service/
  budget_breach event_name/listener_failure mode）；bench_notify 锚点逐字节不变
  （b3 status_events=762、service_events 245/183/244——转移只落日志不发事件）。

### D25 运行时 metrics = 快照面 + 双成本模型（O2）

improve 三.2 要 14 项运行时指标与分桶统计。**裁决：库内快照式采集，不引
prometheus/otel 依赖**（2026-10-25）；导出是宿主接线（Phase 0 OBS 裁决形态）。

- 双成本模型：计数器常开（Relaxed 原子，全部落在冷路径——转移/notify/边接线/
  重试决策/错误分级点，一次 relaxed add 是噪声）；延迟仪器化选项门控
  （`MetricsConfig.latencies` construction-fixed，默认关），因为 `Instant::now()`
  在每次 strict resolve 都跑的路径上不可忽略——门关时计时点只做一次 bool 读。
- 计数器语义：`dependency_rebuild_count` 数的是边接线事件（commit_edges 非空 +
  uncommit），不是 provide 次数——无消费者的 provide 不接线不计数；
  `apply_retry_count` 在 S3 重试决策点；重试中间态不构造 Error（只有 park 的
  fail_load 有），所以 timeout 类错误计数 = 1 而重试计数 = 2，两计数器互补。
- 延迟分桶：固定 log2 八桶（<1us..<1s + 兜底），每项 count/total/max——够答
  "慢路径在哪"，不存逐样本。dispatch 延迟挂在 P2 worker 执行处（Queue 本地），
  其余四项挂 RootState.metrics。
- 快照：`Context::metrics()` 一次 registry 遍历出全部生命周期态分布 + provider_count
  （store 跨条带、一次一锁）+ queue_depth/dropped（Queue::stats）+ 全部计数器。
  快照不是跨并发转移的一致切面（文档写明）。
- 验证：tests/metrics.rs 8 用例（态分布/常开计数器/门控零记录 vs 记录/桶不变量/
  重试与类计数/容量拒弃）。bench_notify 锚点逐字节不变。

### D26 trace id = 构造期 uid 的不可变镜像 + boundary tag 上记录面（O3）

improve 三.3 要"为插件和 fiber 绑定 trace ID"，每个 lifecycle 事件可追一条链
（start / resolve / dispatch / update / provider change / dispose / cleanup /
drain / fail load），跨边界保留 trace id 和 boundary tag。**裁决：trace id 就是
fiber 构造期的 uid（独立不可变字段，永不清除），不另立计数器；boundary tag 走
`Plugin::boundary_tag()` 默认方法上内核记录面，不在 ABI/wit 契约里穿传**（2026-10-25）。

- uid 已是单调、唯一、不复用的 fiber 身份（S1）；另立全局 trace 计数器 = 同一点位
  同值的纯重复。独立字段的唯一理由：S4 在 dispose 时撤收 uid，而 teardown/cleanup
  记录必须仍可 join——"start 到 fail 到 teardown"全链的 join 键恰好在收尾处不能
  失效。trace==uid 保证与 O1 既有记录/事件面零迁移 join。
- `LogRecord` 加 `trace: Option<u64>`（None 仅限无 fiber 语境的全局错误线）与
  `boundary: Option<&'static str>`；六个构造站点全归因（base_record/log_transition
  漏斗带本 fiber 的，quarantine/drain_timeout/listener_failure 带当事 fiber 的）。
- 生命周期 span 复用既有 `duration` 字段：`cache_state` 真实换态时读并重置
  `state_since`（fiber 内私有无竞争），`log_transition` 附「离开态停留时长」——
  Pending→Loading = 等依赖时长，Loading→Active = 装载时长，Active→Unloading =
  服务寿命。`internal/status` 载荷加性补 `"trace"`（消费面按字段访问，计数锚点不受影响）。
- boundary tag：`Plugin::boundary_tag()` 默认 `None`（dyn-safe，存量 impl 零涟漪），
  DynPlugin=>"dylib"、WasmPlugin=>"wasm"；base_record 经 fiber.runtime 的 plugin
  读出——dylib/wasm 插件的每条内核日志自动带 tag，零新日志点、零宿主接线。
  契约（C-ABI vtable、WIT）不动：跨边界 *载荷内* 的 trace 透传需要调用方 fiber
  身份进入调用缝（task-local 或 Invoker 传参，公共面语义变更），归 O7 的
  boundary-call 记录统一设计。公开绑定面 = `Context::trace_id()`（root 为 0），
  插件与宿主据此把自身 trace 写进自有记录。
- 验证：tests/tracing.rs 6 用例（root=0 与 trace==uid、转移 span duration、
  合成 boundary tag 上记录、dispose 后 uid 撤而 trace 存活、status 载荷 trace、
  失败激活全记录同 trace）；dylib/wasm 各一条 tag 映射单测。bench_notify 锚点
  逐字节不变（载荷加字段不改计数）。

### D27 状态事件家族 = internal/ 命名空间 + 单一包络构造器，不建重复通道（O4）

improve 三.4 要统一定义状态事件模型（fiber.created/…/disposed、
provider.changed、service.notified、event.dispatched、effect.cleaned、
drain.started/finished）且每个事件带一致载荷。**裁决：全部以 `internal/` 前缀
落地；`service.notified` = 既有 `internal/service`、`event.dispatched` = 既有
`internal/dispatch`（载荷加性对齐，不建重复通道）；`internal/status` 保留**
（2026-10-25）。

- internal/* 已是框架扩展点的纪律载体（作用域过滤、桥接禁订）；在旁边另起
  `fiber.*` 裸名通道会把同一信息复制到两个命名空间，且热路径（notify/dispatch）
  事件量翻倍。映射关系在模块文档与测试中钉死。
- 新发射点 6 处：fiber.created（registry spawn，spawn invariant 通过后）；
  fiber.<state> 六态（publish_transition，**仅真实换态**——与 status 的
  「当前态快照」语义区分：churn 重发布不发命名态事件）；provider.changed
  （notify 每名循环头——notify 恰是全部 provide 侧变更的唯一漏斗，S6 串行段
  内事件生成序 == 提交序，先 changed 后 service 通知）；effect.cleaned
  （run_one_disposer 实际完成臂；超时孤儿不发——effect 未清，cleanup_timeout
  日志已盖）；drain.started/finished（drain_consumers_measured，仅 touched
  非空——零 consumer 的撤下不是 drain）。
- 一致载荷 = events::system_event 单一构造器：fiber/uid/trace/realm 四字段
  身份包络（无 fiber 语境 = null；有则必带，trace 即 D26 字段）+ 事件 specifics
  合入。internal/dispatch 载荷升级为同一包络 + {mode,event}（发射方 ctx 可用，
  C7 发射侧无追踪语义不变）。
- 成本与契约：全部走 emit_internal（无监听者按名 miss，近零成本）；status 与
  internal/service 发射点零改动 ⇒ bench_notify 锚点（b3=762、service_events
  245/183/244）逐字节保持。观测序仍受 C5（不承诺跨事件名观察顺序；D27 保证的
  是生成序）。
- 验证：tests/state_events.rs 7 用例（created+态家族+包络一致、disposed、
  provided 双态 + 撤下 drain 对、cleaned ok=true、panic disposer ok=false——
  预算臂走 spawn 任务 JoinError 收容、dispatch 包络带发射方 trace、日志面与
  事件面不串）。四关 + 锚点逐字节 + 五 demo 全绿。

### D28 健康面 = Context::health() 快照 + 三个专用信号源（O5）

improve 三.5 要 /healthz /readyz 等端点与 runtime 状态快照。**裁决：HTTP 归
宿主（Phase 0 OBS 裁决重申）；库出 `Context::health() -> HealthSnapshot`，
端点体由宿主组装（examples/health_demo.rs 演示），healthy 默认判定 =
failed 空 && stale 空，宿主可覆盖**（2026-10-25）。

- 计数面（plugin/fiber 五态/live）复用 O2 的 registry 走查形态；P1 锁序纪律
  保持：registry 锁下只取 Arc 快照与原子态，fiber 内锁（error、task_scope）
  在锁释放后取；store 条带三元组与 fiber 表查证分段（一次一锁）。
- **recent errors 环**：挂在 `Metrics`（emit 漏斗签名已含 &Metrics，policies
  无根引用的设计红利）；`ErrorPolicies::emit` 分级后 push——每条内核错误恰好
  入环一次。cap = 64（错误冷路径，固定环无界保留无必要）；RecentError 携
  class/fiber/uid/trace/event/message/at，trace 即 O3 join 键。
- **long tasks = tracked 调用年龄**：CancelScope 加 tracked_since 多重集
  （track_start push / TrackedGuard drop pop），仅 run_tracked/spawn_tracked
  挂——「长任务」的语义对象就是边界在飞调用（与 S3 reap 看门狗同一对象）；
  健康面报持有 fiber + 最久年龄，「多长算长」归宿主阈值。普通 in-task 监听
  不入册（abort-safe 工作随 scope 死亡，不是悬挂风险对象）。
- **stale providers**：store 走查 `(service,label,provider_uid)` 三元组，
  持有者 fiber 态 != Active 即 stale（含 fiber 已亡的槽位，state 报
  Disposed）。按 LIFO 构造真实 stale 窗口仅存在于卸载中途——恰是本面要
  暴露的异常；公共路径无确定性窗口可钉，验收 = 健康世界空 + 走查随行。
- 验证：tests/health.rs 4 用例（健康世界计数+四空、失败插件进 failed 明细
  且环/明细同 trace join、环 cap=64 挤出最旧六条、预算化 bridge 在飞调用
  报 long_task 且完成清零）+ cancel 单测（年龄飞行中增长/完成清零）；
  health_demo 出 /healthz 体。四关 + 锚点逐字节 + 五 demo 全绿。

## 3. 剩余真实待决（产品判断，非工程回避）

**R1 dylib 旧镜像常驻的可接受性**：D5 的推论——每次热替换泄漏一份旧代码镜像（有界：次数×体积）。请确认接受，或给出重载频率/体积上限要求（若不可接受，唯一替代是"dylib 不支持热替换，仅重启换新"——也请裁决）。

**R2 目标平台与 MSRV**：请告知（a）部署 OS/arch（Linux x86_64/aarch64？需 musl 静态？Windows/macOS 是否一等公民）；（b）是否需要交叉出 wasm32 目标的内核组件（我的判断：内核恒为宿主侧，不编译进 wasm——请确认）。

**R3 crate 命名**：`dougong / dougong-plugin / dougong-abi / dougong-wasm / dougong-loader`（contracts crate 名 dougong-abi 是否直观？备选 dougong-contract）。

## 4. 交付方式：一个整体代码库，无分期

v1 的"一期/二期"切分是错误的。体量实证（研究实测）：

| 参照 | 行数 |
|---|---|
| TS cordis v4.0.5 核心 | 2,696 |
| redoop__cordis-rust 内核 | 2,898（+contract/dynhost/greeter 268）|
| tiny-cordis 全项目 | 2,971 |
| helloworldtang__cordis-rust 全项目 | 5,055 |

dougong = 内核（约 3k 行级）+ ABI 契约与 dylib 宿主（数百行级）+ wasm 宿主（1–2k 行级）+ 配置 reconcile（数百行级），**全量数千行，一次性作为一个代码库写完**——不存在"分期"，只有同一代码库内提交顺序（内核与验收测试先行提交，边界与 loader 随后连续交付，全部在首个可用版本内）。capability 策略库不属于 dougong（属于 llmharness 应用层），此前把它列为"二期"是分类错误而非范围裁剪。


## 6. R1–R4 裁决记录（2026-10-20 / 2026-10-21，用户）

- **R1 不可接受** → dylib 插件**不做进程内版本热替换**：每个不同 .so 文件进程内仅映射一次（teardown-only，不 dlclose）；同插件换版本 = 重启进程。配置热更（update()）不受影响（不重映射代码）。
- **R2** 平台 = Linux（一等公民）+ Windows + macOS，支持 musl 静态链接；内核恒为宿主侧，不编译进 wasm。
- **R3** ABI 契约 crate 命名 = `dougong-abi`。
- **R4 工具链纪律**（2026-10-21）：仓库工件一律且仅由 `rust-toolchain.toml` 钉定的工具链（1.99.1）编译；禁止以任何其他工具链构建或替换 `target/` 内的工件（含一次性实验）。MSRV 声明已整体移除（2026-10-21 追记）——未实测过的兼容下限一律不宣称；如需版本证明，走隔离环境（独立 worktree/CI）实测后再写进清单。

## 附录 A：语义等价表（in-process vs dylib-v3，D16/F2）

| 面 | in-process | dylib v3 | 锚 |
|---|---|---|---|
| require 能力检查基准 | fiber 兴趣集（声明注入解析承诺） | 实例 descriptor `inject` 集（常量，激活期捕获进 vtable 闭包） | B2；两级语义（undeclared/inactive）一致，检查对象不同 |
| require 载荷 | 类型化键（`Any` 下转）与动态通道字节并存 | 仅动态通道字节，对宿主不透明（编码归服务契约） | B6 |
| emit 载荷 | `serde_json::Value` | 必须合法 JSON 字节，非法 = `STATUS_INVALID_PAYLOAD` | C2（显式非对称） |
| `internal/*` 事件 | 桥禁订（`on_dispatch` 拒绝） | 出站禁发（`STATUS_INVALID_EVENT`） | C3 |
| 重入行为 | 每 fiber 单驱动循环，同 fiber 无锁可环 | 同线程激活栈；环 = 确定性 `reentrancy_cycle` 错误 | B4 / D-V3-3 |
| teardown 后在途派发 | 监听方集随其 fiber 回滚自然收缩 | 发射方 teardown 不撤销已入队派发（内核任务独立于发射方寿命完成） | C7 |
| 监听方卸载时在途调用 | in-process 监听器在 yield 点中止（cancel scope） | 桥调用只跟踪不中止，卸载等其完成（wasm store 禁弃在途 `call_async`；dylib 续链持实例锁，teardown 取锁本就隐式等待） | D17 / S2；C7 发射方语义不变 |
| require 执行器 | fiber 任务上 await | 调用线程本地 `block_on`（`rt.enter` 供 tokio 上下文） | D-V3-4 |
| 错误形态 | `Error { code, message }`（undeclared_dependency / inactive_dependency） | 状态码 + `{"code","message"}` JSON 详情（同名 code 对齐） | D-V3-5 |
| 解析作用域 | fiber 激活 realm（D15 候选链） | 激活 realm（vtable 闭包捕获同一 ctx，同源） | B3 / A3 |
