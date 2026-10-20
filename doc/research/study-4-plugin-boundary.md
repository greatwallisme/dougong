# Study 4: 插件边界设计输入（dylib/so + wasm）

> 来源声明（用户要求：WASM/WIT 不采信模型记忆）：
> - [S1] wasmtime 技能（.claude/skills/wasmtime，含 references/host-rust.md）
> - [S2] wit 技能（.claude/skills/wit）
> - [P] 生态项目实证（标注 commit）: cordis_wasm@127e1dd、cordis-x@1722a68、nucleus@7268142、redoop__cordis-rust@c50d4ff
> - 版本锚点：cordis_wasm 用 wasmtime 48.0.1 / wasmtime-wasi p2 / wit-bindgen 0.61.1；[S1] 示例用 wasmtime 29 / wit-bindgen 0.36（技能样例滞后于生态，API 面一致）；y0usaf@e776950 用 wasmtime 47。**实现前以当时最新稳定版核对 API。**

## 4.1 wasm 宿主侧要点（来源 [S1]）
1. **组件模型开关**：`Config::wasm_component_model(true)` + `Component::from_file`；核心模块（Module）与组件（Component）API 不可混用。
2. **bindgen! 宏**：`bindgen!({ world, path, async: true, with: {...} })` 生成宿主侧类型化绑定（实例结构 + 导入 trait + 导出访问器）；宿主实现导入 trait + `WasiView`。
3. **每插件一个 Store**：`Store::new(&engine, state)`；Store 持有实例状态，不得跨线程共享（需同步）。dougong 映射：**每 wasm 插件 fiber 一对 (Store, Instance)**，state 携带该插件的调用上下文（fiber-id 等）。
4. **WASI p2 为新项目标准**（[S1] 明示）；`wasmtime_wasi::add_to_linker_sync/async`；`WasiCtxBuilder` 按**能力最小化**配置（不给 stdio/env/fs/network 即全无）。沙箱 = WASI 能力 + world 导入面的并集（[S2]：**"组件无法访问未显式导入的资源"——world 即全部可达权威**，与论文 §6.7 设想一致）。
5. **不可信代码双计量**：`consume_fuel(true)` + `store.set_fuel(n)`（细粒度步数）或 `epoch_interruption(true)` + 后台线程 `increment_epoch` + `store.epoch_deadline_async_yield_and_update(n)`（低开销周期检查，可配合 async 让出）；内存限额 `StoreLimitsBuilder::memory_size(...).instances(1)`。
6. **性能**：`engine.precompile_component(&wasm)` + `Component::deserialize`（缓存 AOT 产物）；Pooling allocator（多短命实例）；cranelift opt level。
7. **async**：`async_support(true)` + bindgen `async: true` + `instantiate_async`/`call_*(&mut store, ...).await`；epoch 到期可 async yield（协作式中断）。
8. 陷阱：trap 分类（StackOverflow/OOB/Unreachable/OutOfFuel）；wasm backtraces 需调试信息；跨实例复用用 `linker.instance`。

## 4.2 WIT 设计要点（来源 [S2]）
1. `package ns:name@semver`；引用外部包必须带 `@version`（确定性解析）→ **dougong 的 world 版本化原生受支持**（WASIp1 之外的 WASI 导入钉 `@0.2.0`）。
2. world 的 import/export 即能力清单；`include world` 可组合基础 world（dougong 可做 `dougong:base` + `dougong:plugin` 分层）。
3. **resource**：带身份的句柄；方法隐式 self；`borrow<T>` 借用不转移，裸类型=所有权转移；析构由 Component Model 追踪 → **registration/lease/handle 类语义（论文的 effect 凭证）可用 resource 建模**。
4. 类型系统：record/variant/enum/flags/option/result/tuple/list；kebab-case；`%` 转义保留字。
5. 工具链：cargo-component（guest 构建）、wit-bindgen（多语言绑定，含 c —— **同一 WIT 可给 C/C++ 侧 dylib 用**）、wasm-tools component wit（从组件抽取 WIT 验证）。
6. **注意**：[S2] 示例未用 WIT 层 `async func`；生态中 nucleus@7268142 使用了 `async func`（自述 WASI 0.3 路线）而 cordis_wasm@127e1dd 用纯同步函数 + wasmtime 层 async。**→ WIT 级 async 的可用性随 wasmtime/wit-bindgen 版本变化，实现时以当版验证；保守做法是同步 WIT + wasmtime async 传输（cordis_wasm 路线）。**

## 4.3 生态三种 wasm world 哲学（来源 [P]，全文见 study-3 §3.5-3.7）
| 维度 | cordis_wasm 全镜像派 | nucleus 极简派 | cordis-x 企业契约派 |
|---|---|---|---|
| 服务调用 | call-service(name+abi-hash, method:u32, bytes) | handle-service(name, method, JSON 字符串) | resource lease.invoke(method, payload) |
| 事件 | 5 模式 + register-listener + dispatch-event + next-token waterfall | subscribe/emit，waterfall 推迟 v0.2（guest 拿不到 handle） | 未在抽样段（侧重 lease/instance） |
| 版本化 | wit-version + service/event **abi-hash** | 契约版本注释 | contract{name, revision, descriptor} |
| 错误 | kernel-error 变体（含 inactive/undeclared/capability-denied 两级解析错误） | result<string,string> | rejected/unknown/refused 三分类 + 清理义务注释 |
| 能力 | descriptor.capabilities: list<string> | — | acquire(kind,args) + lease |
- 共性：载荷 bytes/字符串序列化（**不跨边界传 Rust 类型**）；插件 descriptor 声明 inject/provide（论文 Eq.37 直译）。
- **nucleus 记录的真实难题**：guest 侧 waterfall 中间件需要 host 把"剩余链"以 handle 交给 guest——cordis_wasm 的 next-token: option<u64> 是可行解（host 持链，guest 凭 token 续调）。

## 4.4 dylib/so 侧要点（来源 [P] redoop@c50d4ff + cordis-x@1722a68）
1. **反向 API 原则**（redoop 源码注释）：宿主实现框架，插件仅纯计算导出——避免 cdylib 静态链接双 tokio/双内核副本。
2. 模式：`#[repr(C)]` 导出表 + `#[no_mangle] static EXPORTS` + abi_version 门 + libloading；零依赖契约 crate 供插件侧使用；调用方拥有缓冲区（out_buf/cap/out_len）。
3. **卸载现实**：redoop 明确永不 dlclose（mem::forget），teardown 只清状态；cordis-x 的 abi_reload 示例存在但语义未核（中读未覆盖）。→ **dylib"卸载"应定义为 teardown + 代码驻留；真卸载列为高风险非目标**（待用户确认）。
4. 泛化方向（候选，需用户选型）：
   - A. 纯字节协议（redoop 式）：最简、跨语言、无 unsafe 泛型；代价是每服务手写序列化。
   - B. WIT 单一真源 + wit-bindgen c 导出：dylib 与 wasm 共享同一份 world 契约（[S2] 工具链支持 C 绑定），一致性最好。
   - C. 企业资源模型（cordis-x 式 lease/instance/failure 三分类）：语义最完备、工程量最大。
5. 跨副本识别：TS 用 Symbol.for 全局品牌；Rust 跨 .so 的 type_id 不互通 → 约束"每进程一份 dougong 核心"（插件不静态链接内核）即可回避，契约 crate 模式天然满足。

## 4.5 对 dougong 的设计输入汇总
- **双形态统一契约**：以一份 WIT world 为契约真源（wasm 直接用；dylib 经 wit-bindgen c 或手写 C-ABI 对齐同一语义），配套 dougong 契约 crate（Rust 插件进程内形态）——三形态一套语义。
- world 草图要素（Phase 5 细化）：descriptor{name, version, wit-version, inject, provide, config-schema, capabilities}；host 导出 log/call-service/provide-service/register-listener/dispatch-event(+waterfall next-token)；kernel-error 含两级解析错误 + capability-denied；registration resource。
- 沙箱策略：wasm = paper §6.3 的执法边界（fuel/epoch/memory/WASI 最小能力）；dylib = 可信同权代码（仅 ABI 版本门）。
- 实例生命周期：wasm 侧"替换=新 Store+新实例，旧的 drop"天然安全（对照 [S1]）；结合内核 drain 协议（Thm63）→ 事务式热替换可达成；dylib 侧替换 = 旧 teardown + 新 setup（代码驻留）。
