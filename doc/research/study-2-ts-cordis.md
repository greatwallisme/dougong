# Study 2: TS 版 cordis 源码精读笔记

> 溯源：deepseek-harness @ commit `687f282e86`（2026-10-17），vendor/cordis = **@deepseek-ai/cordis v4.0.5**（TypeScript, ESM）。
> 文件清单（2696 行）：index 16 / service 115 / context 146 / logger 271 / utils 287 / registry 337 / events 352 / reflect 418 / fiber 754。
> 实时追加。

## 总体架构（index.ts）
7 个模块：context / events / fiber / logger / registry / service / utils；外部依赖 @deepseek-ai/cosmokit（Dict、Volatile、Awaitable、defineProperty 等）与 @standard-schema/spec（配置校验协议）。

## context.ts
- `Context` 是**被 Proxy 包装的对象**（`new Proxy(this, ReflectService.handler)`，构造器 return proxy）：属性读取走服务解析器；`extend()/isolate()/intercept()` 生成子 context（原型链视图），不改父。
- `Context.is(value)`：跨 realm / 跨 cordis 多副本的品牌识别，靠 `Symbol.for('cordis.is')` 全局符号而非 instanceof。**→ Rust/动态库对应问题：跨 .so 的类型识别不能用 type_id（每个静态链接副本各有一份），需全局符号式注册或强制共享核心库。**
- 服务挂载：`root.fiber/reflect/registry/events/logger`；根 fiber 构造后 `_disposables.clear()`（内置服务不受根 fiber 生命周期管理）。
- `extend(meta)`：`Object.create(parent)` + own 属性覆盖；`symbols.shadow` 透传。
- `isolate(name, label?)`：子 context 携带**原型链式隔离表** `shadow[name]=label ?? Symbol(name)`——查解析沿原型链找最近项。同一 label 可合并作用域。
- `intercept(name, config)`：原型链式拦截表；解析时根优先合并（Service.resolveConfig：祖先先、base 前置、head 后置；有 Config.merge 用之，否则浅合并）。

## service.ts
- `Service` 基类：构造即 `ctx.reflect.provide(name, this, check)` 注册，随 owning fiber 卸载自动移除。
- 符号协议：`init`（类插件构造后钩子）、`check`（可用性谓词）、`config`（拦截配置类型幻影）、`invoke`（可调用服务，如 `ctx.logger()`）、`extend`、`tracker`、`resolveConfig`。
- `[symbols.filter](ctx)`：**事件过滤**——仅当调度 context 的该服务 isolate 标签 == 服务自身 context 的标签时，服务才接收事件。事件分发尊重隔离边界。
- `Symbol.hasInstance` 定制 instanceof，穿透 proxy 链。

## registry.ts
- 插件三形态：Function `(ctx,config)` / Constructor / Object `{apply}`；归一为 callback（**callback 即插件身份键**）。
- 插件元数据 `Plugin.Base`：name、`Config`(StandardSchema 校验器)、`inject`(名称数组或 名称→拦截配置 映射)、`provide`(服务名)、`intercept`(声明消费哪些服务的拦截配置)。
- `Inject` 装饰器：类级累积到静态 inject（原型继承）；方法级=延迟到服务可用后执行（经 `ctx.inject` 包裹，注册进 initHooks）。
- `Plugin.Runtime`：每 callback 一份共享记录 `{name, fibers: DisposableList<Fiber>, Config}`；**一个插件可有多 fiber**（每次 ctx.plugin() 一个）。
- `RegistryService`：`_internal: Map<Function, Runtime>`；`counter` 读即自增（fiber uid 分配器）；`plugin()`：resolve→当前 fiber assertActive→建/复用 Runtime→`new Fiber(ctx, config, Inject.resolve(inject), runtime, outerStack)`→包装为 PromiseLike（await→fiber.await()）。
- `ctx.inject(deps, cb)` = `plugin({inject, apply: cb})`；`delete(plugin)` = dispose 其全部 fiber 并删记录。

## events.ts
- 分发五模式：`emit`（同步，忽略返回）/ `parallel`（全等待，AggregateError）/ `serial`（顺序 await 至 bail）/ `bail`（同步短路）/ `waterfall`（围绕 `next` 续延的洋葱组合，不调 next=否决）。
- `isBailed(v)`：v ∉ {null,false,undefined}。
- 类型化事件面靠 `declare module` 增强 `Events` 接口；**internal/\* 事件是框架扩展点**：plugin(创建/销毁)、status(状态迁移)、config(waterfall，注入就绪后解析配置)、service(绑定拦截)、update(waterfall，配置更新可否决——HMR 挂点)、get/set(waterfall，包装 proxy 读写)、listener(bail，可替换注册)、dispatch(诊断)。
- Hook = `{ctx, callback, prepend?, global?}`；`dispatch()`：取 thisArg→`filter = thisArg[Context.filter]`→过滤 `hook.global || !filter || filter(hook.ctx)`→bind thisArg。
- `on()`：assertActive→`reflect.bind(listener)`→`internal/listener` bail 可拦截→`register()`=**当前 fiber 的 effect**（push hook，返回反注册闭包）→随 fiber 卸载自动移除。
- 单一 `_hooks: Record<name, Hook[]>`（挂在根 events 服务上），可见性靠 filter，不按 context 分桶。

## fiber.ts（核心，754 行）
### 状态机（6 态）
`PENDING（等依赖）→ LOADING（回调运行中）→ ACTIVE ↔ UNLOADING（disposers 运行中）→ PENDING/…`；`FAILED`（回调/配置抛错）；`DISPOSED`（uid=null，不可复活）。

### 关键字段
`uid`（0=根；null=已处置）/ `ctx`（=parent.extend({fiber:this})）/ `config`（校验后）/ `_config`（原始，每次激活前重解析）/ `store`（**已提交视图 ω**：激活时 `_store` 快照，Dict<Impl>）/ `inertia`（在途迁移 Promise）/ `_disposables`（**累加器 g**：DisposableList）/ `_store`（候选依赖实现，按注入名）/ `_runner.epoch`（**目标指纹**）。

### epoch 机制（对应论文 target/quiescence）
- `_refresh()`：epoch = `'' + ':' + impl.fiber.uid`（按注入名顺序拼接各提供者 **fiber uid**）；任一缺失 → `INACTIVE`。**提供者身份指纹：提供 fiber 被处置（uid 置 null）即指纹变化，即使新提供者"值相等"**。
- `_setEpoch(epoch)`：相同→noop；否则若 `inertia` 在途→**只改指纹不启动迁移**（落地后由 `_updateState` 对比处理=惯性/divert）；无在途→`INACTIVE→非INACTIVE` 启动 `_reload`（LOADING），否则 `_unload`（UNLOADING）。
- `_checkImpl(name)`：reflect._getImpl 解析 + check 谓词 → 写/删 `_store`。

### 迁移（对应论文 reload/unload 链）
- `_reload()`：`store = {..._store}`（**提交快照 ω**）→ `await Promise.resolve()`（微任务检查点）→ **epoch 未变才执行**（过时即放弃，状态更新会排干 PENDING 期间收集的 effects）→ 解析配置（internal/config waterfall + schema 校验）→ `_execute(runner)`；成功清 `_error`，失败记 `_error`、epoch=INACTIVE。收尾：epoch 仍等于旧值→`inertia=undefined`（ACTIVE）；否则**链式 `_unload`**（UNLOADING）。
- `_unload()`：`_disposables.clear()` 后 **Promise.all 并发执行**（每个 disposer 错误隔离记日志）→ `store=undefined` → epoch 仍 INACTIVE 则 INACTIVE，否则**链式 `_reload`**。注意：**顶层 effect 间并发清理，单 effect 内部逆序串行**（effect() 本地 dispose 逆序链式 await）——论文 LIFO 在 effect 内严格执行，顶层按独立性假设并行。
- `await()`：循环等 inertia，重抛 `_error`；`restart()` = setEpoch(INACTIVE)+_refresh()+await（卸载后立即重载）；`update(config)` = internal/update waterfall（HMR 可否决/替换）后 restart。

### effect()（对应论文 effect 迭代器 + 事务激活）
- effect 体四种返回形态：`Disposable` / `Promise<Disposable>` / `Iterable<Disposable>`（生成器：逐个收集）/ `AsyncIterable`（**异步生成器：每次迭代前检查 runner.epoch，epoch 变则停止收集=L-Divert**）。
- 本地 disposables 逆序串行 dispose；`disposing` 幂等；`effectInertia: WeakMap<Disposable, () => Promise>` 允许外部加入他人已启动的清理（单次消费但可等待）。
- wrapper 挂 `symbols.effect` 元数据（EffectMeta{label, children} 树）供 `getEffects()` 诊断。
- 注册即推入 fiber 级 `_disposables`（可被重入的父卸载看见）；执行失败→回滚已收集+reject setup。
- 状态非 ACTIVE/UNLOADING 时 `INACTIVE_EFFECT` 错误。

### fiber 作为父的 effect（级联）
- `this.dispose = parent.fiber.effect(..., 'ctx.plugin()')`：**子 fiber 的整个生命周期是父 fiber 的一个 effect** → 父卸载级联子退役（论文 Eq.31 递归 context 的实现）。
- dispose 体：uid=null→emit internal/plugin（错误隔离）→从 runtime.fibers 移除（空则 registry.delete）→setEpoch(INACTIVE)→若 inertia 在途则 await 之（while 循环）。PENDING 期已有 effects 的显式卸载处理。
- 发布次序：disposer 完全就绪后才 emit internal/plugin（同步观察者可能立即 dispose 子/父）；发布后才 `_checkImpl`×N + `_refresh()`（父非 UNLOADING 时）。

### 通知（对应论文 Eq.26）
- `_updateState`：状态变更 emit internal/status；**仅 ACTIVE↔非ACTIVE 边沿**时，对本 fiber 提供的每个 key 调 `reflect.notify([name])`——依赖者重新 _checkImpl+_refresh。

### 配置
- 激活时 `internal/config` waterfall（thisArg=fiber）→ runtime.Config 校验（StandardSchema，同步）→ ValidationError。
- `update()`：ACTIVE 时先 `_resolveConfig`（可能访问注入服务），再 internal/update waterfall 决定是否 restart（noSave 提示持久化钩子）。

## 待 reflect.ts/utils.ts 确认
- store（值表）结构、provide/impl 记录（fiber/value/check）、notify 实现、reflect.bind、Proxy handler 的 get/set 拦截与错误分类（inactive vs undeclared）。
- DisposableList 语义（clear 返回什么顺序）、symbols 清单、getTraceable/joinPrototype/createCallable/withProps、composeError/buildOuterStack。
EOF
echo "study-2 initial: $(wc -l < /home/greatwallisme/project/dougong/doc/research/study-2-ts-cordis.md) lines"
## reflect.ts（418 行）——服务解析与 store
- **store 按隔离标签分桶**：`store: Dict<Impl, symbol>`（键=该名字的 isolate label 符号，非名字本身）；`Impl = {name, fiber, value, check?}`；`props: Dict<Property>`（service/accessor 两类声明）。
- Proxy get 陷阱解析顺序：特殊属性（symbol/prototype/then/数字串/下划线开头）直通→自有属性直通（traceable 包装）→accessor 钩子→非插件 context 走非严格 get→**internal/get waterfall**→解析游走：`fiber.store[prop]` 命中即返回（traceable 包装）；`prop in fiber.inject` 但无实现→"inactive context" 错（**已声明未激活**）；isolate 标签不匹配→错（隔离边界）；到根仍无→"without inject" 错（**未声明**）。两级错误区分与论文/Alg6 一致。
- `reflect.set(name, value)`：仅**提供 fiber 本人**可写（`impl.fiber === ctx.fiber`），原地覆写 value，不触发通知——提供的服务值是可变单元；提供/撤回走 provide 的 effect，与 set 分离。
- `provide(name, value, check)`：fiber.effect 实现——登记 props；**根 isolate 表全局登记 label**（`root[symbols.isolate][name] ??= Symbol(name)`）；同 realm 同名重复提供→抛错（**每 key 每 realm 唯一提供者**，论文基线假设）；写入 store[label] **并同时写入自己 fiber 的 store**（激活期间自见）；若已 ACTIVE 立即 notify。**disposer = 撤回协议**：删 store[label]→`notify([name])`→**await 所有被刷新 fiber settle（Promise.allSettled）**→最后删自己 store[name]——**drain 内嵌在 provide 的逆操作里**（论文 Thm63 实现）。
- `notify(names, filter)`：**线性扫描 registry 全部 fiber**：注入名匹配且 isolate 标签匹配者 `_checkImpl`+`_refresh`；随后按名 emit `internal/service`（携带过滤 thisArg）。**无反向依赖索引**——fiber 数量大时是 O(N·M)；dougong 需权衡索引。
- accessor/mixin：计算属性与 ctx 方法转发（ctx.on→events.on），均为 fiber effect。
- trace/bind：proxy 重绑 `ctx` 与参数到调用方 context（环境上下文魔法）。**Rust 无对应也不需要——显式传 ctx**。

## utils.ts（287 行）
- `DisposableList`：Map<sn,T>+WeakMap 反查；push 返回反注册闭包；**clear() 返回逆序数组**（_unload 顶层并发清理按 LIFO 起始顺序）。
- `symbols`：全部 `Symbol.for('cordis.*')` 全局注册表符号——跨副本识别的机制基础。
- 其余：isConstructor（原型检测）、joinPrototype（原型链融合）、getTraceable/createTraceable（服务方法调用时重绑 ctx 的代理层）、withProps/createShadow（this 影子）、composeError/buildOuterStack（异步长栈拼接）。

## logger.ts（271 行，抽样）
- 标准服务形态：构造器 super(ctx,'logger') 注册；可调用服务（[Service.invoke]）；named logger 工厂。内核设计无关紧要，作为服务实现范例。

## Phase 2 总结：TS cordis 完整机制图
1. **两个全局表**：reflect.store（label→Impl，服务实现）+ registry._internal（callback→Runtime，插件运行时）。
2. **两条身份链**：fiber uid（提供者身份，epoch 指纹）+ isolate label 符号（作用域身份，store 分桶与事件过滤）。
3. **effect 单一变异原语**：ctx.effect → fiber._disposables 累积；provide/on/plugin 全部构建其上；provide 逆操作内嵌 drain-await。
4. **迁移 = epoch 对比驱动的惯性状态机**：_setEpoch →（无在途才启动）_reload/_unload；落地后 epoch 不符链式反向迁移；inertia 期间只记指纹。
5. **异步生成器 effect** = 论文 effect 迭代器的实现（逐步收集逆 + 每步 epoch 检查）。
6. **通知 = ACTIVE 边沿 + notify 线性扫描**。
7. **JS 专属魔法（Rust 不移植）**：Proxy 属性解析、traceable this 重绑、原型链 shadow、callable 服务、全局符号品牌。Rust 对应物：显式 get/inject、类型键、trait。
