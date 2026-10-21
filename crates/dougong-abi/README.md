# dougong-abi

dylib 插件与 dougong 宿主之间的稳定 C-ABI 契约（`#[repr(C)]`、零依赖）。任何
独立编译的 `.so` 插件只需依赖本 crate：导出 `DOUGONG_PLUGIN_EXPORTS` 表、
通过调用方持有的缓冲协议读写、以 `ABI_VERSION` 门完成版本校验。

事件拉链采用 CPS 协议（v2）：`on_event` 返回 `STATUS_DEFERRED` 请求续链，
宿主经 `on_next` 回注结果。契约变更必须 bump `ABI_VERSION`（设计决策 D14，
见仓库根 [doc/research/dougong-design.md](https://github.com/greatwallisme/dougong/blob/master/doc/research/dougong-design.md)）。
