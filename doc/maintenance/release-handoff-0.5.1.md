# v0.5.1 发布交接说明（2026-11-04）

本版是 PATCH 缺陷修复：dylib 拆除（disposer/teardown）与插件自有线程出站
require 的锁序倒置——0.5.0 中「disposer 持实例锁跑 teardown join」与
「worker 出站 require 排队等同一把锁」互为前提，环不可解，首次全量真共载
（下游 fly-rafter Phase 18a，20 dylib 工件 + 信号关停）以 ≈50% 概率关停
挂死。修复 = 停摆纪律（需求冻结规格：`doc/maintenance/teardown_requirements.md`，
实现依据其 R1-R8）：

1. disposer 两臂：liveness 门翻转到锁获取**之前**；获取保持普通
   `lock().await`（tokio Mutex 公平队列保证有限步收敛：排队等待者逐个
   获锁→撞锁内门→`provider_gone` 退场，disposer 的轮次到达时队列必空）。
2. 出站 `require` 与事件桥新增预锁 alive 检查（门翻后的新到达者在排队前
   失败——无新等待者）。
3. 事件桥补上锁内 alive 检查（0.5.0 桥无门 = teardown 后进入已释放
   vtable 的既有 UB 洞，一并封口）。
4. `dispatch` 入口新增早门（门翻后即刻失败；在途 dispatch 经链内监听者
   在门处退场自然收敛，无锁参与不成环）。

## 发布核对

- 五 crate 同步 0.5.1（仅 dougong-dylib 语义变化，其余为版本同步）；
  0.5.0 工件不受影响（无版本门变化，接口面零变更）。
- 语义零变更承诺：`provider_gone` 状态码与文案、`STATUS_OK` 路径、
  reentrancy 纪律（D-V3-3）、同一实例外来入口不交错的串行化保证逐字节
  不变；loader JSON、wasm/WIT、CPS 协议、descriptor schema 不变。
- 新增回归钉测：`teardown_joins_a_requiring_worker_without_deadlock`
  （插件自有 worker 循环 dispatch、链路进入被拆激活自身事件桥，与
  teardown join 竞争同一把实例锁，限时收敛断言；require 面受 descriptor
  自注入死锁约束无法构造，见钉测注释。已实证旧码挂死 / 新码收敛）。

## 下游 fly-rafter 交接

- 依赖同步升 `dougong* = "0.5.1"`；多自有线程插件（fr-storage facility、
  fr-tools-builtin notice/schedule、skill worker 等周期性出站 require 的
  工件）无需任何代码变更——停摆纪律由宿主侧统一保证，插件 teardown 的
  join 纪律（v3 契约）照旧。
- 实证：修前 fr 18a 信号关停 ≈50% 挂死（线程图三 worker 全部停在实例锁
  futex）；修后 boot 5/5 稳定、SIGINT 130 / SIGTERM 0 界内收敛、全量
  真共载六场景集成测试绿。

## 验证凭证（复现命令）

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo build -p greeter-wasm --target wasm32-wasip2
cargo run -p dougong-dylib  --example dylib_demo
cargo run -p dougong-dylib  --example outbound_demo
cargo run -p dougong-wasm   --example wasm_demo
cargo run -p dougong-loader --example loader_demo
cargo run -p dougong-loader --example harness_demo
cargo run -p dougong        --example health_demo
```

2026-11-04 全部实测：四关零警告零错误；workspace 测试 241 passed / 0 failed
（含新增回归钉测）；六 demo exit 0（wasm demo 先建 wasip2 guest 工件）。
