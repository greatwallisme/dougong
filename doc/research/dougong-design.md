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

### D8 事件五模式全量进契约（不延后）
waterfall 跨边界 = 宿主持剩余链 + next-token（cordis_wasm 已建模；nucleus 的"guest 拿不到 handle"难题由此解）。emit/parallel/serial/bail/waterfall 一并规范；沙箱侧并发预算由 fuel/epoch 约束（[S1]）。

### D9 声明式配置与 loader 的真实切分
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


## 6. R1–R3 裁决记录（2026-10-20，用户）

- **R1 不可接受** → dylib 插件**不做进程内版本热替换**：每个不同 .so 文件进程内仅映射一次（teardown-only，不 dlclose）；同插件换版本 = 重启进程。配置热更（update()）不受影响（不重映射代码）。
- **R2** 平台 = Linux（一等公民）+ Windows + macOS，支持 musl 静态链接；内核恒为宿主侧，不编译进 wasm。
- **R3** ABI 契约 crate 命名 = `dougong-abi`。
