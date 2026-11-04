# dougong 升级需求文档：dylib 激活拆除的出站停摆纪律（teardown quiescence）

> 状态：定稿（2026-11-04）。
> 行号基准：本仓 9bb3b0b（v0.5.0 tag）。缺陷实证来源：fly-rafter Phase 18a
> 首次全量真共载（20 个第一方 dylib 工件 boot + 信号关停面）的挂死线程图与
> ≈50% 复现率；本文档先于实现冻结，release-handoff-0.5.1.md 于实现与测试
> 收口后另写。

## 1. 背景与目标

### 1.1 现状七事实（本提案的全部依据）

1. disposer 在实例锁内运行 teardown：`let _instance =
   teardown_lock.lock().await` 之后才 `alive.store(false)` 并调用
   `teardown_exports.teardown(&teardown_handle)`（dougong-dylib lib.rs
   disposer 两臂：registered effect 臂与 inline fallback 臂，同纪律）。
2. 出站 require 持同一把实例锁整段调用：`DylibInvoke::call` =
   `enter_exclusive(&lock, instance_id).await` → alive 门检查 →
   EntryGuard → `call_full`（lib.rs:1527-1536 附近）；门检查在获锁**之后**。
3. 事件桥同锁：`event_bridge` 的 `enter_exclusive` 跨 waterfall 续体全程
   持锁（lib.rs:1568 起）。
4. teardown 纪律（导出表契约，v3 起成文）：插件必须在 teardown 返回前
   join 全部自有线程；teardown 返回后的 vtable 调用 = 文档化 UB（D5/R1）。
5. alive 门语义（D31 修正）：require 在解析到 invoker 后遇上拆除 →
   `provider_gone`（transport 级）快速失败，禁止重试进死句柄；该门由
   disposer「锁内、teardown 前一刻」翻转。
6. dispatch v5（D32）入口不持自身实例锁：B6 姿态门拒绝外来帧后，在插件
   自有线程上驱动内核同名派发面至链收敛；链内监听者 require 派发者服务 =
   普通依赖边放行（dispatch_requirements.md §3 E1.9）。
7. 插件自有线程（工件 spawn 的 OS 线程）经出站 vtable 周期性 require：
   fr 实证三处（storage facility、tools-builtin notice/schedule、skill
   ticker），require 频次与 boot 后时间窗相关。

### 1.2 术语

- **实例锁（InstanceLock）**：逐激活的 `Arc<tokio::sync::Mutex>`，全部外来
  入口与出站 require 的串行化点。
- **liveness 门（alive）**：激活上的原子布尔；false = 拆除中/已拆除。
- **外来入口**：setup / call / on_event / on_next / teardown（EntryGuard 帧）。
- **插件自有线程**：插件工件 spawn 的 OS 线程（其出站 require 以该工件自身
  的实例锁为 caller 串行化点）。
- **在途持有者**：已通过全部门并持有实例锁执行 C 调用的线程。
- **排队等待者**：已发起出站入口、正等待实例锁的线程。

### 1.3 缺陷路径（锁序倒置环）

disposer 持实例锁 → teardown（锁内）→ 插件 join 自有线程 → 该线程的出站
require 正在排队等同一把锁（caller 串行化点 = 被拆实例自身的锁）→ join 永
等 → runtime 线程永阻 → 依赖 runtime 轮询的一切机制（async 定时器、超时、
看门狗的 async 臂）失效。实证线程图：disposer、storage facility、
tools-builtin notice 三线程全部停在同锁的 futex 上；关停挂死 ≈50% 复现
（worker 周期 require 命中拆除窗口即成环）。关键不对称：**排队等待者的
退场条件是「获锁后见门翻」，而 disposer 的退场条件是「join 到该线程」，
两者互为前提**——当前代码中该环无解。

### 1.4 路径裁定

- **路径一（否）：插件侧自修。** crate 只能在 teardown 内 quiesce 自有
  线程，而 teardown 必须先获锁；在途 require 的竞争窗口在 teardown 之前
  即成环，crate 侧无解。
- **路径二（否）：出站 require 不持实例锁。** 重入检测（D-V3-3）与同一
  实例外来入口不交错的串行化保证坍塌；语义破坏不可接受。
- **路径三（本提案）：disposer 侧两段式停摆。** 门先翻、锁后取、取锁与
  等待者交错——排队者在获锁那一刻见到门翻即退场（provider_gone），在途
  持有者在各自 C 调用边界内自然完成；disposer 必然在有限轮内无竞争获锁，
  teardown 起跑时**不存在任何排队等待者**，join 不可能成环。

### 1.5 目标

- G1 (MUST) 任何插件自有线程的在途/排队出站入口都不得使 teardown join
  永阻——拆除链在有限时间收敛；
- G2 (MUST) 语义零变更：`provider_gone` 状态码与详情文案、`STATUS_OK`
  路径、reentrancy 纪律（D-V3-3）、「同一实例外来入口不交错」的串行化
  保证逐字节不变；
- G3 (MUST) 无新 UB 窗：teardown 执行期间任何外来入口不得进入（门先于
  锁翻 + 锁内旧检查保留，双保险）；
- G4 (MUST) 零回归：既有测试矩阵（dispatch_requirements §3 E1 系、grant
  G 系、E1.10 setup 探针、并发钉测、六 demo）全绿；
- G5 (MUST) 新增确定性回归钉测：构造「worker 在途 require 竞争 teardown
  join」复现（旧代码挂死、新代码限时收敛）；
- G6 (SHOULD) 版本 0.5.1（semver PATCH 缺陷修复），五 crate 同步。

## 2. 需求条款（每条自带理由与实现要点）

- **R1 (MUST) disposer 预翻转**：`alive.store(false, Release)` 先于任何
  实例锁获取执行。理由：排队等待者唯一的退场通道是「获锁后见门翻」；门
  翻得越早，等待者越早退场。实现要点：disposer 两臂（registered 与 inline
  fallback）的翻转语句均上移到 `lock().await` 之前。
- **R2 (MUST) disposer 普通排队获取**：预翻转后仍用普通
  `lock().await`（tokio Mutex 公平队列：lock 按请求顺序授予）。理由：
  disposer 之前的每个排队等待者获锁即撞 R4 锁内门、以 provider_gone
  退场，轮到 disposer 时队列必空——有限步收敛，无需任何轮询/睡眠/忙等。
  反向鲁棒：即便调度不公平使 disposer 先获锁，teardown 后到达的等待者
  也撞锁内门安全退场（不 deadlock、不 UB）。实现要点：`lock().await`
  原样保留，唯一变化是翻转语句的位置。
- **R3 (MUST) 出站入口预锁门**：`DylibInvoke::call` 与 `event_bridge` 在
  `enter_exclusive` **之前**增加 alive 检查（false → `provider_gone`，
  与锁内既有检查同码同文案）。理由：阻止「门翻之后新排队」的等待者产生，
  从源头保证 R2 的收敛前提；快速失败亦免除无谓排队。
- **R4 (MUST) 全入口锁内门检查齐备**：既有 call 路径「获锁后、call_full
  前」的 alive 检查不动；**event_bridge 补上同款锁内检查**（0.5.0 现状
  桥无任何门——排队桥在 teardown 后获锁会进入已释放的 vtable，属既有
  UB 洞，本提案一并封口）。理由：预锁检查与获锁之间存在窗口（排队期间
  门才翻），锁内检查是该窗口的封口，也是排队等待者退场的执行点，与
  D31 修正的 provider_gone 语义同源。
- **R5 (MUST) dispatch 入口早门**：`dispatch_impl` 在 B6 姿态门之后增加
  alive 检查（false → `provider_gone`，STATUS_FAILED 类）。理由：门翻后
  新发起的 dispatch 即刻失败；门翻前的在途 dispatch 由「链内监听者对被
  拆实例的 require 在门处退场 → 链以失败收敛 → dispatch 返回」自然了断
  （无锁参与，不成环）。
- **R6 (MUST) 两臂同纪律**：registered effect 臂与 inline fallback 臂均
  实施 R1+R2。理由：两臂共享同一把实例锁，任何一臂保留旧锁序即保留环。
- **R7 (MUST) 回归钉测**：确定性复现——stub 出口 `call` 故意慢（数百毫
  秒），独立线程循环 require，主线程驱动 uninstall；断言 uninstall 在限
  时内完成且 require 以 `provider_gone` 退场。理由：G5；旧代码在本测下
  永阻（超时即红），新代码限时收敛（绿）。
- **R8 (SHOULD) 文档同步**：dylib-plugin.md 的 teardown 纪律节补一句
  「disposer 与排队出站入口的停摆次序（门先翻、锁交错取）」；kernel 技能
  文档（dougong-plugin-dev）pitfall 索引同步。

## 3. 测试矩阵

| # | 场景 | 断言 | 对应条款 |
| --- | --- | --- | --- |
| T1 | 在途 require 竞争拆除（慢 call + worker 循环 + uninstall） | uninstall 限时收敛；worker require 以 provider_gone 退场 | R1/R2/R7 |
| T2 | 排队 require 竞争拆除（worker 获锁前门翻） | require 以 provider_gone 退场，状态码文案不变 | R1/R3/R4 |
| T3 | 门翻后新发起 require / emit / dispatch | 各自快速失败（provider_gone / 既有 emit 语义 / dispatch 早门） | R3/R5 |
| T4 | 既有矩阵：E1 dispatch 14 项、grant G 系、E1.10 setup 探针、并发钉测 | 全绿零回归 | G4 |
| T5 | 六 demo（dylib/outbound/wasm/loader/harness/health） | exit 0 | G4 |

## 4. 验收与验证凭证

四关（fmt / clippy -D warnings / test / doc -D warnings）+ 五 demo 全绿；
T1 为新增回归钉测；fly-rafter 侧以 0.5.1 复验其 18a 信号关停面（SIGINT
130 / SIGTERM 0 / 关停 5s 界内收敛）作为下游实证。
