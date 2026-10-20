# Study 3: Rust 参照项目对比研究

> 溯源规则：每项目记录 HEAD commit + 阅读日期（用户要求：引用必须注明版本/commit）。
> 参考库根：~/project/reference/cordis/（32 仓库，2026-10-20 克隆）。

## 3.1 redoop__cordis-rust @ `c50d4ff`（2026-08-25，crates.io: cordis-rust v0.2.0）★7

**定位**：DSH cordis 内核的直接 Rust 移植（自称"Same mental model, same lifecycle grammar"）。tokio 单运行时，serde_json::Value 无类型配置。**对 dougong 最有价值的参照**：语义对齐 TS + 含 dylib 宿主先例。

### 结构（workspace 5 crates）
- `cordis`（内核 ~2.9k 行：context 566 / fiber 861 / events 409 / service 346 / plugin 211 / error 206 / logger 151 + tests：accessor/events/isolate/reactive）
- `plugin-contract`（零依赖 C-ABI 契约 crate）
- `greeter-plugin`（纯 C-ABI cdylib 示例插件）
- `dynhost`（libloading 宿主 + ForeignPlugin 适配器）
- `cordis-plugin-timer`（示例插件）

### 内核 API 形态
- `Context` = 便宜克隆句柄 `{root: Arc<RootState>, scope: Arc<ScopeNode>, fiber: Weak<Fiber>}`
- `ScopeNode` = **持久链表**（parent: Option<Arc<ScopeNode>> + isolate: HashMap<name,label:u64> + intercept: HashMap<name,VecDeque<Value>>）——TS 原型链的 Rust 直译；`effective_label` 沿链找最近
- `RootState`：registry / reflect store / events / fibers: HashMap<uid, Weak<Fiber>> / root_fiber / counter / label_seed / **active_fibers: AtomicU64 位图**（uid<64 快速 ACTIVE 判定，≥64 回退锁路径）
- 服务：字符串名 + **`Arc<dyn Any + Send + Sync>` 类型擦除**，`get::<V>()/require::<V>()` downcast；`provide[_with_check]` async；`accessor(name, compute)` 计算型服务（每次读取以读取方 context 重算）；`set` 仅提供者本人可写
- 事件：名 + `serde_json::Value` 载荷；五模式齐（emit/parallel/serial/bail/waterfall）+ **同步槽优化**（on_sync/emit_sync 零任务派生，为 per-token 流式热路径设计）+ parallel_timeout；once 用 AtomicBool 包装
- 插件：`trait Plugin {name/inject/apply}`；`plugin_with(name, injections, closure)`；`ctx.inject(deps, cb)`；配置 = serde_json::Value + resolve_config
- effect：`ctx.effect(label, async { Ok(Some(disposer)) })`；`Disposer = Box<dyn FnOnce() -> BoxFuture<()>>`；EffectGuard{Weak<Fiber>, id} 单次幂等
- fiber 状态机：Pending/Loading/Active/Failed/Unloading/Disposed（repr(u8)）；**state_cache: AtomicU8 原子镜像**（热路径无锁读）；收敛用**每 fiber 一个 tokio driver 任务**（drive_loop：desired/loaded/dirty/force；unload→load 循环收敛，比 TS 链式 inertia 更直观）
- epoch：字符串指纹 `":" + ":<provider_uid>"×N`（与 TS 相同语义）；提供者 dispose 时 uid=None 使指纹失效
- join()/join_with_timeout()（防 apply 永不完成楔死调用方——实践补丁）/restart()/update()（internal/update waterfall 可否决=HMR 挂点）
- 锁次序文档化：root.reflect < registry < fibers < fiber 内部锁；通知两阶段（快照后出锁刷新）

### 插件边界（dylib 先例，对 dougong 最关键）
- **反向 API 原则**（源码注释明言）：勿跨边界传 `Arc<dyn Plugin>`——cdylib 会静态链接自己的 tokio/cordis 副本（双运行时、"no reactor running" 恐慌）。**宿主实现框架语义，插件只做纯计算**。
- 形态：插件 = cdylib 导出 `#[no_mangle] static cordis_plugin_exports: PluginExports`；`#[repr(C)] {abi_version: u32, name(buf), setup()->handle, produce(handle,buf)->i32, teardown(handle)}`；调用方拥有缓冲区协议；插件只依赖零依赖契约 crate（无 cordis 无 tokio）。
- 宿主：libloading::Library（Arc 持有保映射）→ ForeignPlugin 适配为 Arc<dyn Plugin>，apply 内 ctx.effect→provide，disposer 回调 teardown。
- **卸载策略：故意永不 dlclose**（`std::mem::forget`）——teardown 只清状态，代码映射保留至进程退出。**热卸载未解决**（注释承认"ForeignPlugin may outlive any safe unmap point"）。
- ABI 版本检查：宿主拒载 abi_version 不匹配。

### 与 TS/论文的偏差（dougong 决策参考）
1. **无父→子级联**：子 fiber 不注册为父 effect，无自动级联卸载（论文 Eq.31/TS dispose=parent.effect 未移植）；靠显式 dispose/stop（stop 按 newest-first）。
2. 服务/事件均为字符串名 + Any 擦除 / Value 载荷——**放弃了 TS 的类型化 Events/Context 表面**；类型安全靠调用点 downcast。
3. 顶层 disposers **串行 LIFO**（TS 是顶层并发）；无 effect 迭代器（apply 单个 Future，无逐步收集/D-Livert 语义——只有 effect 粒度）。
4. notify 仍线性扫描全部 fiber（无反向索引）。
5. 隔离 label 用 u64 而非 symbol；拦截配置 VecDeque 分层合并。
6. 无 interceptor 元数据 monoid（拦截=配置合并）；无 HMR（模块级）；无 accessor 之外的特殊键。
7. 值得抄：driver-loop 收敛、状态原子镜像、ACTIVE 位图、join_with_timeout、同步槽事件、锁次序文档、plugin-contract/dynhost 全套。

### 对 dougong 的启示
- dylib 插件形态的最小可行模式已验证：**零依赖契约 crate + C-ABI 导出表 + 宿主适配器 + ABI 版本门**。但该样例契约面极窄（name/setup/produce/teardown）；dougong 需要泛化的"服务值如何跨界"（序列化 vs 共享内存 vs handle 传递）——**开放问题**。
- dylib 热卸载在生态内尚无安全方案（redoop 直接放弃）——dougong 的 dylib 是否要求可卸载=**开放问题**（倾向：teardown-only，代码驻留）。
- 级联缺失是明显语义退化：dougong 若要忠实论文/TS，需设计 Rust 式父→子级联（如 spawn 时在父 fiber 注册 EffectGuard），**成本与收益=open question**。

## 3.2 jincdream__cordis-rs @ `85036f6`（2026-08-21）★2 — 语义对齐清单（最重要参照之一）

**定位**：cordiverse/cordis 全包族的 Rust 移植，**冻结于上游 commit `8cc9e33f`**；10 crates（core/utils/timer/logger-console/loader/include/group/hmr/create）。
- **163 场景可追溯矩阵**：每个上游 `it()` 映射到具名 Rust 测试（docs/upstream-test-map.md / upstream-scenario-evidence.md / upstream-api-map.md）——**dougong 的语义验收清单可直接采用**。
- Rust 化替代 Proxy/装饰器：typed handles `Context / ServiceRef<T> / TrackedService<T> / Associated<T>`；`Context::property/set_property/intercept_chain/intercept_value/associated/get_tracked/invoke/inject`；明言"intentionally avoids emulating unrestricted JavaScript property access in Rust"。
- loader（readiness 谓词/自更新持久化/typed partial updates/递归配置插值/incremental reconciliation/**stable realms**/迁移）、include（YAML/JSON `!!js` 经宿主注入 ConfigEvaluator，文件监视/事务替换）、hmr（依赖传播/序列化批次替换/构建回滚/**原生编译用 PluginFactory 工厂**）。
- **局限：插件全部编译期内置（PluginFactory），无 dylib/wasm 边界**。dougong = 此项目语义清单 + redoop 边界工程。

## 3.3 mloong-agent-kernel @ `b1f2d36`（2026-09-01）★1 — 冻结契约的大规模内核（25.7k 行，**轻读：结构级**）

cordis-core（effect/event/id/invocation/lifecycle/plugin/service）+ cordis-runtime（含 benches）；tokio + tokio-util。README 自述"Kernel design contract 已冻结，MSRV 1.85"。价值：比 redoop 更完整的运行时工程化参照（性能基准、结构化并发、关闭收敛）；细节留待设计阶段按需深读。

## 3.4 tiny-cordis @ `4254a1e`（2026-10-16）— 最小教学内核（2,971 行，**轻读**）

tokio 最小特性集；context/fiber/kernel/scope/resource/plugin + 生命周期/回滚测试 + rust_microkernel_design.md 设计文档。价值：最小语义骨架样例（约 3k 行即可承载 scope/fiber/resource/unwind），dougong MVP 的体量参照。

## 3.5 cordis-x @ `1722a68`（2026-10-18）★1 — 多边界运行时（113k 行，**中读：边界协议**）

crates: cordis-abi（examples: abi_composition/context/managed/reactive/**reload**/**rolling**/library）+ protocols/component/wit/plugin.wit。Rust/C++/TS 三 SDK；本地 C-ABI 库 + WASM 组件 + 独立进程四种插件载体（README）。
- WIT `cordis:plugin@0.1.0`：企业级契约——`contract{name, revision, descriptor: payload}`；`failure = rejected(business) | unknown | refused(runtime)`（注释：rejected=业务错误、refused=运行时拒绝、unknown 不可降级；**任何清理错误都保留清理义务**）；`resource lease{invoke, release}` + `resource instance{activate, invoke, acquire(kind,args), deactivate, close}`；payload=list<u8>（"业务消息占 bytes 字段，控制元数据永不 JSON 化业务数据"）。
- 价值：**rolling update / reload 的边界协议样例** + 失败分类学（与论文 §6.1 acquisition/emission 义务语义对齐）。113k 行是重量级实现，细节按需再读。

## 3.6 cordis_wasm @ `127e1dd`（2026-09-04）★1 — Component Model 全内核镜像（15.6k 行，**中读：WIT**）

wasmtime **48.0.1**（async/component-model/cranelift/runtime）+ wasmtime-wasi p2 + **wit-bindgen 0.61.1**；自述"cordis@4.0.0-rc.9 TS 实现的 Rust 原生重写，语义对齐"，论文 arXiv:2608.25512 为理论依据；trybuild UI 测试 + `component` 属性宏。
- **WIT `cordis:kernel@0.1.0`**（guest/host 双份）：
  - `host`（插件导入）：`call-context{fiber-id, effect-id}`、`service-id{name, abi-hash}`、`event-id{name, abi-hash}`、`event-mode` 五模式枚举、`kernel-error{invalid-argument, inactive-context, inactive-dependency, undeclared-dependency, capability-denied, internal}`（**两级解析错误 + capability 拒绝进 ABI**）、`registration` resource；函数 log / call-service / provide-service / register-listener / dispatch-event。
  - `plugin`（插件导出）：`plugin-descriptor{name, version, wit-version, inject, provide, config-schema: list<u8>, capabilities}` + descriptor/activate(ctx,config)/deactivate/call-service/handle-event。
  - world `cordis-plugin { import host; export plugin; }`。
- 设计要点：载荷 list<u8>；**service/event ID 带 abi-hash（§6.6 接口漂移的工程化解法）**；waterfall 用 next-token 续延。
- **同项目另见 seago__cordis-rs/crates/cordis-wasm/wit/cordis.wit、collab-ai-dev__cordis-rs/wit/cordis-plugin/world.wit、spatiotemporal/crates/spatiotemporal-wasm/wit/plugin.wit**（共 8 份 WIT，设计分歧点已记录，Phase 4 对比）。

## 3.7 nucleus（meritozh）@ `7268142`（2026-08-26）— WASI 0.3 极简面（**中读：WIT**）

自述"cordis paradigm (from deepseek-harness) 的 Rust 实现 + WASI 0.3 components 动态插件"。
- WIT `nucleus:plugin@0.1.0`：**极简 JSON 字符串 RPC**——`init(config: string) -> result`（async）、`dispose-effect(handle: u64)`、`handle-service(service, method, arg: string) -> result<string,string>`；host 侧 register-effect/subscribe/emit/call-service；**waterfall 明确推迟到 v0.2 并在 design.md §8 记录原因**（"guest 拿不到可下放的 handle"）。
- 价值：最小 wasm 契约样例 + "guest 侧 waterfall 中间件"难题的真实记录（dougong 的 waterfall-over-wasm 需正面回答此问题：cordis_wasm 的 next-token 是一种解法）。

## 3.8-3.13 轻读档案（README/结构级，深读按需）

| 项目 | commit | 定位与要点 |
|---|---|---|
| y0usaf__cordis-rs | `e776950`（2026-08-17） | cordiverse/cordis 重实现；**wasmtime 47**；插件=WASM 模块；tests 含 wasm_compose/config_wasm。wasm 编排参照 |
| eavae__cordis-rs | `3a114bf`（2026-08-23） | cordisjs 非官方实现：宿主读 YAML 配置→加载**动态库插件**→解析服务→管理 reload/unload；dylib+声明式配置组合样例 |
| arcships__rutis | `4d989c40`（2026-10-20，★148 活跃） | 多语言插件（TS/Python/Go/Swift）跨进程热重载；crates.io+npm 双发布。工程成熟度最高；进程级边界与 dougong 的进程内+wasm 形态不同，借鉴其热重载与分发 |
| weibaohui__cordis-rs | `d7e28da`（2026-08-15） | cordis 5 概念（Plugin/Context/Event/Effect/HMR）Rust 移植，crates.io |
| tr1v3r__cordis-rs | `a2a0365`（2026-09-26） | cordis v4 运行时：协调器/effect/服务/事件 + **JSON loader/reconcile**；HMR 未做（自述 P9） |
| wanwanhhh__cordis | `9f40a01`（2026-09-05） | Builder（装配期 &mut）/Context（Clone+Send+Sync 只读句柄）/Runtime（唯一所有者）的 Rust 所有权化设计——**所有权建模参照** |

## Phase 3 中期结论（供 Phase 5 决策草案）
1. **语义清单**：以 jincdream 的 163 场景矩阵 + study-1 论文清单为 dougong 验收基线。
2. **dylib 形态**：redoop 反向 API 原则 + 零依赖契约 crate + ABI 版本门 = 已验证模式；缺口在"服务值跨界"泛化（redoop 只传 bytes；cordis-x 有 lease/resource 模型可参考）与卸载策略（全生态无人 dlclose）。
3. **wasm 形态**：wasmtime 48 + wit-bindgen 0.61 是当前栈；三份 WIT 哲学（全内核镜像 cordis_wasm / 极简 JSON nucleus / 企业契约 cordis-x）需用户选择或混合；abi-hash、capabilities、next-token waterfall 是共性要点。
4. **共通短板**（生态空白，dougong 的机会）：父→子级联普遍缺失/弱化；reconcile 完整实现少（tr1v3r 有 loader/reconcile、jincdream 有 incremental reconciliation）；wasm 插件的 drain 语义未见完整实现。
