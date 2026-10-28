# v0.4.0 发布交接说明（2026-10-28）

本版只含 ABI v3 → v4 的最小破坏性升级：`HostExports` 追加 `grant`，以及
dougong-dylib 的激活级授权表。设计裁决见 D31，迁移清单见 CHANGELOG 0.4.0。
descriptor schema、loader JSON、wasm/WIT、五事件模式与既有 require/emit 语义
不变。

## 发布核对

- 四 crate `dougong` / `dougong-abi` / `dougong-dylib` / `dougong-loader` 同步
  0.4.0；v3 工件由既有版本门硬拒，错误含两侧版本。
- 静态 inject 命中的 require 不触授权表；无授权名条目的静态未中路径保持
  UNDECLARED 状态与详情字节不变。
- 授权目标为已提交激活而非服务名；授予者 teardown/再激活清理全部条目，新激活
  不继承。无 revoke、通配、方法粒度、转移、再委托、wasm 或 in-process 动态授予。
- `Invoke::activation_id()` 是内核唯一新增身份助手，`BoundaryInvoke` 必须转发；
  不得用解析所得装饰器 Arc 地址代替激活 id。

## 下游 fly-rafter 交接（E4）

- 依赖同步升 0.4.0；策略插件 `provide` 自有决策服务、`inject` 静态声明 tools，
  setup 先 `grant(target=tools, granted=自有服务)`，再 require tools 的注册方法。
- fr-tools 的 descriptor `inject` 不得加入任何策略服务名；新增第三方测试策略
  插件时 fr-tools 任何字节都不改，以此验证通用注册面。
- 目标 tools 换激活会经静态注入级联使策略插件重新 setup、重新 grant；不得依赖
  旧授权跨激活保留。

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

dylib 重点锚：`cargo test -p dougong-dylib`（ABI v3 回归 + ABI v4 E1.1–12）；
`outbound_demo` 的 `grant arm: authorized callback round trip verified`。
下游 E4 另需 fly-rafter workspace 四门全绿及第三方零改接入钉测。
