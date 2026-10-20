# Study 1: 论文精读笔记 —《A Programming Paradigm for Spatiotemporal Composability》

> 源文件: doc/research/cordis.md（arXiv:2608.25512, Shi/Zhang/Cui, PKU & DeepSeek-AI, 88 页）
> 本文件实时追加，[x] 标记已精读部分。

## 阅读进度
- [x] §0 总纲 + §1 引言 + §2 预备 + §3.1 Revertible Effects + §3.2.1 Coeffect context（行 1-675）
- [ ] §3.2.2 通知 + §3.3 Context 范式 + §4 演算
- [ ] §5+ 实现与案例（reconciliation/HMR/Koishi）

## 核心主张（§0-§1）
1. **时间可组合性**：组件离场时，其对共享环境的修改可被撤销，不留残留，不破坏期间无关工作。
2. **空间可组合性**：依赖出现/消失/替换时，消费者连贯反应：满足才激活、运行期解析稳定、提供者最终撤离前先行停用。
3. 两维**正交**：清理≠依赖协调，反之亦然。
4. 组件模型：`Component = requirements(D) + provisions(P) + effects-with-undo(E*)`
5. **fiber** = 存活组件实例（演算的核心对象）
6. 粒度匹配论点：effect 撤销与依赖协调应与组件同粒度（进程内），进程重启/服务拆分都太粗。
7. 生产案例：Koishi（4000+ 社区插件）；cordis 为参考实现。

## Effects/Coeffects 视角（§2）
- effect ↔ 环境修改；coeffect ↔ 环境需求（对偶）
- 论文动作：把静态 effect/coeffect 标注 **reify 为运行时可操作 context 对象**——这是全篇的关键一跳，Rust 实现应理解为"运行时对象"，而非类型系统设施。

## Revertible Effects（§3.1）数学要点
- Eq.4 twisted composition：`(f1,g1)∘(f2,g2) = (f1∘f2, g2∘g1)` —— **逆序反转**，teardown 天然 LIFO。
- Eq.5 effect context：`∂Γ = Γ × (Γ→Γ)`（当前状态 γ + 恢复累加器 φ）。
- Eq.6 tracking：前向变更把自身逆追加进累加器：`track(f,g)(γ,φ) = (f(γ), φ∘g)`。
- Eq.9-11 recovery：`recover(γ,φ) = (φ(γ), id)`；健全性不变量 `φ(γ)=γ0`（后弱化为观测等价 `≃`）。正确追踪的 effect 不改变最终恢复结果。
- Eq.12 **witnessed effect**：`e(γ)=(δ,g)` 且 **g 在应用时发现**并满足 `g(δ)=γ`（例：注册监听返回 disposers、打开资源返回句柄）。→ Rust 形态：操作返回"逆操作闭包"，逆操作在应用时捕获具体凭证。
- Eq.13 `◇` 组合成 monoid（Thm10/11：witnessed 子幺群）。
- Eq.14-16 lifting 到 effect context；Thm16：**LIFO 选择性回退**，中间态保持健全不变量。
- §3.1.3 独立性：Eq.17 变换幺群 M(e)；Eq.18 交换条件（前向与逆全部互相交换）；Eq.19 逆稳定性（他者的变换不改变本 effect 产生的逆）；Thm20 交错序列中**摘除单个 effect = 该 effect 从未发生**且其余保留；Cor21 成对独立下逆可**任意排列**恢复原状。
- 实现意义：跨组件 effect 独立性在实践中靠 **按键隔离**（每 effect 只动自己的 key）达成，见 §3.2.1 Eq.22-23（操作限制在自己的 key 内）。

## Coeffect context（§3.2.1）
- Eq.20 `Σ = (k:K) ⇀ V_k`：**带类型的偏映射**（key→typed value），即 DI 容器的数学形态。
- Eq.21 `set(k,v)` 本身是 **witnessed revertible effect**（其逆 = 删除该键）——发布依赖与撤回依赖是同一被追踪操作的两半。**get/set 即 DI 的全部基础**。
- Eq.22-23 每 key 的操作带 (新值, 逆, 产出)，提升到全 context 时只动本 key → 天然满足跨 key 独立性。

## 对 dougong 的 Rust 映射备忘（Phase 1 中间记录，待 TS/Rust 阶段校验）
1. 撤销累加器 φ 的工程形态 ≈ **LIFO undo 闭包栈**（Vec<Box<dyn FnOnce>>）或等效 disposer 收集器；须与 TS 实现核对（TS 可能以 fiber 收集 disposers 而非函数复合）。
2. witnessed effect → `FnOnce(ctx) -> Disposer`；Disposer 应用时捕获具体凭证（id/token/句柄）。
3. key 隔离是实现独立性定理的工程手段 → 类型键注册表 + isolate 作用域（与多个 Rust 参照项目的"服务 = 类型键注册表 + isolate"表述吻合，Phase 3 验证）。
4. `set` 即 effect → DI 的 provide/withdraw 必须走同一 effect 通道，不可绕过。
5. 观测等价 `≃`：健全性最终以"可观测状态等价"为准 → Rust 中指语义等价而非内存相等，测试策略需基于可观测行为。

## 待后续章节回答的问题
- Q1 通知机制如何定义（§3.2.2 reactive classification）？
- Q2 fiber 状态机与依赖门控的精确语义（§4）？
- Q3 reconcile 与 HMR 如何落在演算上（§5）？
- Q4 isolate/realm 的形式定义（影响 Rust 所有权设计）？

## §3.2.2–§3.2.3 通知、隔离与拦截（行 676-800）
- Eq.26 **通知三分类**：`notify_d(σ,σ')` ∈ {activating（从不满足→满足）, deactivating（从满足→不满足）, neutral}。边沿触发，对每次 context 变迁逐一核对需求集。局部保证**不足以**决定提供者撤离顺序 → 需全局演算（§4）。
- §3.2.3 **隔离（isolation）**：Def27 区分 **in-place**（原地修改+非平凡逆）与 **derived**（父 context 不动，派生子 context 携带 override，丢弃子即恢复）。
- Eq.27-28 **realm 两级映射**：`Σ_iso = (K⇀R) × (R⇀V)`，逻辑键 k → realm ρ(k) → 值。`isolate(k,r)` 运行时改路由。"运行时特设多态"：同一逻辑依赖在不同 context 指向不同具体绑定。
- Eq.29-30 **拦截（interception）**：每 key 一个元数据 monoid `(M_k, ⊕_k, ε_k)`；`intercept(k,ν)` 累加拦截元数据，`get(k,μ) = σ(k)(μ⊕ι(k))`。拦截改变的是**能力如何被使用**而非由谁提供（§6.3 用于 capability 访问控制）。

## §3.3 统一 Context 范式（行 801-950）
- Eq.31 **统一递归 context**：`Γ∞ = μΓ. Γ × (Γ→Γ) × Σ`——(状态, 逆累加器, 依赖表) 三元递归；父 context 拥有子 context，卸载父时聚合移除子 effects。Σ 可编码任何需要中介的共享可变状态，不止服务依赖。
- Eq.32 观测等价：dom 相等 + 每 key `≃_k`；Def34 用有限测试定义不可区分；**Lemma38：此前全部恢复定理在观测等价商上依然成立**。
- Eq.35 独立性扩展到产出（pr₃）；**Thm40：不同 key 上的操作结构性地独立**。
- key 分两类：**可交换 key**（registry 型，独立增删）与**不可交换 key**（有序 middleware 链）。→ dougong 需在类型上区分这两类 key 语义。
- Eq.36 + Thm42：coeffect 介导的多段 effect（下一步依赖上一步产出）在不相交 key（或可交换同 key）下独立。**结论：coeffect 结构为交错下的时间可组合性提供独立性**。
- §3.3.3 局部性原则：effect 与逆**同处声明**；需求**一次声明**；全局清理/重接线由 runtime 推导。组件作者只写局部事实。

## §4.1 组件与 fiber（行 952-1028）
- Eq.37 组件 `(d, p, e)`：需求键集 / 可提供键集 / witnessed effect 函数。
- **基线假设：同一 realm 内不同 fiber 的 provisions 不相交**（每 key 唯一提供者；isolation 可按 realm 放宽）。
- Eq.38 生命周期 `Inactive | Active(g, ω)`；**ω = 已提交的提供者视图**（commit 时冻结的 provider 身份快照）。
- fiber 记录 `⟨d,p,e,π,σ,τ,θ⟩`：父 id、自有提供表、退役标记、生命周期态。
- Eq.39 registry：名字→fiber，原子新鲜身份（Rust: u64/AtomicU64 id 生成器）。
- Eq.40 **有效依赖环境 = 所有 Active fiber 提供表的并**——派生视图，**不存在独立权威注册表**。这是重大架构取向：提供表是事实源，容器是派生。

## §4.2 基本演算（行 1029-1077）
- Eq.41 **target**：`target_n(γ) = ⊥`（若退役或需求不满足）；否则 d_n 每 key → **provider 身份**（不是值！）。身份区分对"替换"至关重要：值相等 ≠ 生命周期相同。
- Eq.42 **quiescence**：实际生命周期态 == 解析出的 target 态。即 **desired-state reconcile**（K8s 式，fiber 粒度）。
- 编排规则 O-Insert/O-Retire/O-Remove + 生命周期规则 L-Reload/L-Unload；**编排器只表达存在/退役意图，激活态由规则从 target 推导**。"期望组合是输入，激活状态是派生"。

## §4.3 进行中的迁移（行 1078-1290）——工程上最关键的一节
- Eq.43 完整生命周期：`Inactive(ζ) | Reloading(i,g,ω) | Active(g,ω) | Unloading(g,ω,ζ)`；ζ=失败原因/退役理由（Eq.44 failed 判定）。
- Eq.45 进行中的迁移不算 quiescent。
- Eq.46 **relied(n)**：存在其他已安装 fiber m，其已提交视图 ω_m 的某 key 指向 n → **drain 协议**：
  ```
  ACTIVE →(L-Leave)→ UNLOADING（不再对外可见）→ 等 relied(n)=false → 执行累加器 → INACTIVE
  ```
  消费者在自身 teardown 期间仍经**已提交视图 ω** 读到提供者。组件粒度的依赖感知排水。**dougong 必须实现 reversed 依赖索引（谁提交的视图引用了我）**。
- Eq.47-48 **effect 迭代器**：多步激活，每步产出 (新态, 本步逆, 续延)；中断时 LIFO 部分回滚；生命周期增 L-Begin/L-Iter/L-Finish/**L-Divert**（激活中途 target 变化 → 在迭代边界停下，回滚已安装部分）。
- §4.3.3 **异步惯性（inertial）**：飞行中的迭代必须落地，落地后记录其逆，若 target 已过时则**链式进入 unloading**。reload→unload、unload→reload 均可链式；绝不让过时的异步完成把瞬态提供者暴露为 ACTIVE。
- §4.3.4 Eq.49 **失败感知迭代器**：`Either(Ξ, ...)`；失败本身无新增 effect，但此前成功步骤的逆已累加 → 进入 unloading 恢复。**事务式激活**（非 try/catch 约定）。

## Rust 映射备忘（追加）
6. ω（已提交提供者视图）→ `Arc` 冻结快照：`key → (fiber_id, handle)`；消费方持有快照，与提供方退役解耦。
7. target/quiescence → 纯函数 reconcile 循环：intent(insert/retire/remove) + 当前 registry → 每 fiber 的 target → 生命周期机收敛。事件驱动 + 边沿通知（Eq.26）。
8. relied() → **反向依赖索引**：consumer 提交视图中被引用的 fiber_id 集合的逆映射；在 L-Leave 时查询。
9. effect 迭代器 → 插件激活是多步异步流程且每步可逆；Rust 形态候选：async 步进 + 每步 disposer 入 LIFO 栈；divert = 检查代际/版本号后中止。具体如何映射到 TS 的 apply/provide 调用，Phase 2 核对。
10. realm 两级路由（Eq.27-28）→ Rust: `LogicalKey → RealmId → Value`；intercept 元数据 monoid → 每 key 的中间件/策略叠加（capability 控制留待后续，最小实现可只做 realm 路由）。
11. "可交换 key vs 有序 key" → dougong 事件/中间件键需显式声明有序性，影响独立性判定与并发策略。
12. 有效环境=Active 提供表并集（Eq.40）→ 不设中心注册表；派生解析在 reconcile 时计算并冻结进 ω。

## 待后续章节回答的问题
- Q3 reconcile 与 HMR 如何落在演算上（§5）？
- Q4 isolate/realm 的形式定义（§6?）与 capability（§6.3）？
- Q5 Koishi/cordis 实现与演算的对应（§5-7？）

> 溯源：本文档基于本地固定文档 doc/research/cordis.md（arXiv:2608.25512 v1，88 页全文 Markdown/LaTeX 重建版）。

## §4.4 元理论（行 1351-1604）
- Def58 良构 registry 不变量：父必存在；同 realm 内 provisions 不相交；已安装 fiber 的已提交视图对其声明需求**全定义**且指向已存在 fiber；视图中每个提供者自身已安装。Thm59 保持性。
- Thm61/62（Eq.56-57）**时间可组合性全局形式**：执行 n 的累加逆 ≈ "只发生过其他 fiber 的步骤"；fiber 离场后无任何可观测残留。
- Thm63（Eq.58）**空间次序**：嵌套生命周期——提供者激活先于消费者；消费者 episode 结束先于提供者完成卸载；期间绑定稳定可读。
- Thm64（Eq.59）**解析一致性**：一次迁移只对着一个已提交解析跑；target 中途变化 → 要么一致完成要么 divert+回滚，绝不新旧依赖混用。
- Thm66 进展：优先关系 `n≺m ⟺ p_n∩d_m≠∅` **无环**前提下，非静默态必有可用规则（无死锁）；有限名字+迭代长度上界 K 下终止 `S(n)≤(K+4)(V(n)+1)`。**drain 守卫的死锁风险由无环性排除**。
- Thm73 **合流性**：同一编排输入下，任意合法迁移历史的最终静默可观测态 == 从零规范装配的终态。**动态历史不会永久污染终态**——自修改运行时的核心期望性质。

## §5 Cordis 实现映射（行 1606-1784）——dougong 的直接蓝本
- **理论↔cordis 对应表**：Γ∞→ctx；effect 提升→`ctx.effect(cb)`；Σ→context store；get/set/isolate/intercept 同名；fiber；需求→`fiber.inject`；提供→`provide`；激活→`fiber.apply`；父→`fiber.parent`；态→`fiber.state`；**累加器 g→`fiber.dispose`**；ω→`fiber.committed`；target→`fiber.target`。
- Alg1 effect 追踪：`ctx.effect` 是**唯一变异原语**；每步 yield 逆闭包，runtime 复合为一个 disposer；**armed 守卫**使 disposal 幂等并阻止后续 effect 迭代；**子 context 的 disposers 复合进父**（递归 effect context）。明确局限：**不机械证明用户逆的正确性**（局部义务交给组件作者）。
- Alg2-3：存储 = 值表 + realm 表 + 拦截元数据；`ctx.set` 经由 `ctx.effect` 实现 → **提供自动可逆**。通知：扫描受变更 key 影响的存活 fiber，key+realm 均匹配其声明注入才 `refresh`。提供者进 UNLOADING 先于物理移除（Thm63 实现）。
- Alg4-5 生命周期：`ctx.use(component,config)` 创建 fiber 且**本身是父 context 的被追踪 effect** → 父卸载级联子退役。`refresh`：无在途迁移才启动 reload/unload。reload：快照 target→提交解析→跑 effect 迭代器→累积恢复→target 未变则 ACTIVE，否则**链式 unload**。unload：通知/排干依赖者→等其 INACTIVE→执行 disposer→清 committed→target 仍缺席则 INACTIVE，否则**链式 reload**。互链=惯性异步。
- **target 存 provider 身份**：等值新实例替换仍触发依赖者 reload。
- Alg6 context 访问：JS Proxy 包装 ctx.get；沿 fiber-parent 链解析：已提交→放行；已声明未提交→inactive 拒绝；未声明→undeclared 拒绝。既是依赖强制也是类 capability 安全模型。**Rust 无 Proxy → 必须显式 `ctx.get::<T>(key)`/注入字段，dougong 的关键分歧点**。
- §5.2 加载器：声明式期望状态（Def74 字段 id/url/isolate/intercept/config/disabled）；reconcile 把变更映射为最小扰动操作（url 变→重建；disabled→卸载；intercept→原地更新；isolation→realm 迁移——Eq.65 delimiter 标签判定绑定随迁）。合流性保证 reconcile 只看最终配置。
- §5.2.2 HMR（Alg8-10）：变更模块分类 accepted/declined（环默认 declined）→ 闭包定点扩展；陈旧条目=依赖闭包∩accepted；**事务式模块重载**：失效缓存(留备份)→dispose 旧 fiber→import/实例化新→失败则恢复缓存重实例化旧。**fiber 边界即 HMR 接受边界**（effect 有界可逆）。
- §5.3 Koishi：4000+ 插件验证表达力与通用性。

## §6 讨论（行 1786-1966）——对 dougong 的直接告诫
- §6.1 **系统边界**：acquisition（open/malloc/fork→可追踪句柄）vs **emission**（写文件/发包/扣款→真逆不存在）。两策略：**withholding**（提交前扣留外发）、**compensation**（应用级补偿）。cordis 不是外部世界的魔法回滚。
- §6.2 **服务多路复用**：独占绑定 vs **service broker**（消费者见稳定 broker，后端提供者来去）。滚动更新=加新→ACTIVE→切流→排干旧→卸载。跨进程提供者接口必须异步。
- §6.3 **访问控制**：依赖声明=capability 模型；intercept 做能力衰减（fs + {read:/repo, write:none}）；provider 不改、策略来自 context。**明确声明：语言级中介不是抗恶意代码的沙箱**；真实隔离需 SFI/独立 runtime/独立进程/**VM/WebAssembly 边界**。两层：capability context(策略) + sandbox boundary(执法)。→ **wasm 插件形态的理论正当性直接来自此处**。
- §6.4 **语言无关性**：需要闭包（逆捕获恢复状态）+ 运行时代码引入/撤回（native=动态链接；**WASM=宿主控生命周期**）；类型侧需类型化依赖声明 + 运行时中介（proxy/反射/宏）。范式不绑定 TypeScript。
- §6.5 互相依赖：环 → 永久不满足（可从声明检测，非调度死锁）；建议拆 core+integration 组件；代价是组件爆炸（最坏平方级），用打包/约定装配/脚手架缓解。
- §6.6 **依赖类型化与版本化**（形式模型的实际短板）：接口漂移 & key 碰撞；三策略：命名空间(K×P)、peer 依赖（**cordis 现行路线**）、结构兼容谓词（行为契约难/不可判定）。**"公共能力协议不应依赖裸字符串 key"**。
- §6.7 语言/OS 协同设计：编译器可把 effect 迭代器编译成状态机、coeffect 声明进类型系统（编译期环检测/结构兼容）；OS 协同=声明依赖即**全部可达权威**（如同 **WASM 模块的显式 imports**）。

## §7 相关工作（行 1969-2025，未完）
- vs ZIO/Effect-TS：cordis 是宿主语言普通代码上的**覆盖层**（非 monad 内编码），且撤销=重跑显式逆+重解析，非换解释器。
- vs 可逆计算：只需**应用点单侧逆**，弱于全局可逆。
- vs 生命周期回调（清理手写不全）、框架 disposables（有局部性无聚合）、RAII/线性类型（静态作用域）、内核扩展恢复（最近系统先例，但 cordis 允许组件自定义 effect 词汇）、初始化期 DI（Spring/Guice/Angular/React context 无移除/替换异步生命周期）、OSGi DS/iPOJO（可用性响应式，最近先例）……

## Rust 映射备忘（追加）
13. armed 守卫 → disposer 状态位（幂等 drop + 阻断后续 effect）。
14. fiber.dispose = LIFO disposer 收集器；子 disposer 上卷父 → 卸载级联。g 不是函数复合而是**线性收集**。
15. **Proxy 属性访问无 Rust 对应**：dougong 用显式 `ctx.get::<K>()` / 生成注入字段；解析拒绝分两级（inactive vs undeclared）。
16. HMR：TS 依赖模块注册表失效；Rust dylib 热卸载有 TLS/符号泄漏等深坑（须查证），wasm 实例替换天然安全。**dylib 热重载支持等级 = 开放问题**。
17. broker 模式与滚动更新协议留给 llmharness 插件层实现，内核只需保证 drain 语义。
18. capability(策略层) vs sandbox(执法层)：**wasm=执法边界；dylib=可信同权代码**。dougong 文档需明确此区分。
19. 版本化：进程内 Rust 类型键=编译期结构兼容；跨 dylib 需稳定键 ID/ABI 版本；wasm 走 WIT 接口（Phase 4 以现行文档定）。key 命名空间化(K×P)采信。

## §7.4 末-§8 结论（行 2026-2061）
- vs 值级响应式（FRP/signals）：正交可组合——coeffect 值本身可以是响应式的（组件级生命周期 × 值级信号叠加）。
- 结论链：revertible effects + reactive coeffects → unified context → component+fiber 演算 → 全局恢复/依赖排序/进展/合流 → cordis 实现。**自进化 agent harness 被点名为未来验证目标**。

## §9-11（行 2063-2176）：公式索引(1)-(65)、定理-收益对照表、算法 1-10 映射表（检索用，略）。

## §12-16（行 2179-2695）：【转写者 ACRYL 视角的引申，非论文正文——引用需注明】
- §13 建议直接采纳 10 项：effect ledger（逆优先变异 API）、显式 requires/provides、target 含 provider 身份、四态生命周期（INACTIVE/LOADING/ACTIVE/UNLOADING + 失败）、drain 为内核规则、分步激活+回滚检查点、期望态 reconcile、事务式 HMR、context realms、intercept 元数据。
- §14 不应盲抄 5 项：勿假定一切 effect 可逆；勿以裸 key 相等为公共兼容协议；勿把 capability 当安全沙箱；勿未原型就把整个应用塞进 cordis（最小内核清单：ctx.effect/provide/resolve/isolate/intercept + fiber 生命周期 + reconciler）；勿忽视独立性假设（**偏好细粒度能力/状态所有权，而非多组件共改一个全局大袋**）。
- §15 验证实验模板：4 类可热换 provider + 10 个场景（替换时排干、半途失败恢复、并发独立变异、realm 隔离、realm 级拦截策略……），成功判据=时间(移除A≈历史无A)/空间(单一提交视图)/进展(收敛静默)/合流(同终态配置同终态)。**dougong 的验收测试可直接以此为蓝本**。
- §16 终局：**持久的是 context/场景；actor/provider/tool/扩展都是被 reconcile 的 fiber，effect 有界、依赖显式。**

## 阅读完成
- [x] 全文 2695 行读毕（2026-10-20）。
- Phase 1 结论：论文给出了完整的内核语义规范——dougong 的理论需求清单即 §13 的 10 项采纳 + §14 的 5 项告诫；验收标准取 §15。
