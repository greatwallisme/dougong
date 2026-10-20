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
