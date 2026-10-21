# dougong-dylib

dougong 的 dylib/so 插件宿主：`libloading` 映射 + ABI 版本门 + 动态通道适配，
把 C-ABI 插件适配为内核 `Plugin`。宿主以每实例锁串行全部跨界调用；插件镜像
进程内常驻（teardown-only，不 `dlclose`——设计决策 D5/R1）。

配套示例：`examples/dylib_demo.rs`。信任模型与沙箱对照见
[仓库根 README](https://github.com/greatwallisme/dougong)。
