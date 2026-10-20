# Findings: GitHub cordis 相关 Rust 项目

## 搜索方式
- API: `https://api.github.com/search/repositories?q=cordis+language%3Arust&per_page=100`（经 proxychains4）
- raw JSON: `/tmp/cordis_search_p1.json`
- `total_count=51`，单页即取全（51 items），无后续分页
- 全部 51 项均 `fork=false`、`language=Rust`（服务端过滤）

## 结果（2026-10-20 查询）

| # | full_name | stars | 描述摘要 |
|---|-----------|-------|----------|
| 1 | dshbox/cordis-rs | 59 | 长生命周期插件化 Rust 应用的类型化运行时 |
| 2 | Chinesezjc/CordisClaw | 66 | (无描述) |
| 3 | redoop/cordis-rust | 7 | DSH cordis 库的 Rust 移植，一切皆插件内核 |
| 4 | redoop/dsh-rs | 11 | DeepSeek Harness 的 Rust 移植，基于 cordis-rs 内核 |
| 5 | dirmacs/ares | 18 | 基于 Cordis 框架的可组合 AI agent 运行时 |
| 6 | eavae/cordis-rs | 16 | cordisjs/cordis 的非官方 Rust 实现 |
| 7 | eric8810/rutis | 21 | Cordis 核心范式的地道 Rust 实现 + 最小 agent 框架 |
| 8 | weibaohui/cordis-rs | 4 | Cordis (cordiverse/cordis) 的 Rust 实现：DI+EventBus+Effect+HMR+Plugin |
| 9 | seago/cordis-rs | 3 | Cordis 的 Rust + Wasm 参考实现 |
| 10 | wavetao2010/tessivum | 4 | 受 DeepSeek Harness 与 Cordis 启发的 Rust agent harness |
| 11 | rootkiller6788/dsh-launcher | 25 | DeepSeek Harness 插件化本地 AI 运行时平台 |
| 12 | meritozh/nucleus | 2 | 受 cordis 启发的动态插件系统 |
| 13 | P0lar1ght/dock | 3 | 本地 agent TUI：Cordis 插件树 + MCP 工具 |
| 14 | jincdream/cordis-rs | 2 | Cordis 插件框架的独立行为兼容 Rust 移植 |
| 15 | y0usaf/cordis-rs | 2 | Cordis 元框架的 Rust 重实现（Lua 脚本） |
| 16 | arcships/rutis | 148 | 自迭代软件的插件运行时（多语言插件热重载） |
| 17 | d-vid04/CordisChat | 0 | 加密群聊 |
| 18 | wanwanhhh/cordis | 0 | (无描述) |
| 19 | mloongx/mloong-agent-kernel | 1 | 基于 rust 与 cordis 模式的 agent 内核 |
| 20 | morluto/leantoken | 114 | 面向 agent 的代码智能，精简上下文 |
| 21 | ckhawks/cordis | 0 | Discord 风格语音/视频/文字聊天桌面应用 (WIP) |
| 22 | zhouyunsheng31/cordis | 1 | Cordis 研究分支：学习与记忆系统，含 Rust 状态树内核 |
| 23 | HiruNya/cordis | 0 | 玩具级 Discord Bot 框架（不可用） |
| 24 | miko-p/MikoLauncher | 2 | Minecraft 启动器：Cordis 插件范式 + Tauri/Rust |
| 25 | ashuai/localai | 2 | DSH 式 cordis 插件架构的本地 LLM Rust 实现 |
| 26 | aimbrace/cordisrs | 0 | (无描述) |
| 27 | stevenke1981/cordis-rs | 1 | Python CORDIS 认知运行时的 clean-room Rust 重写 |
| 28 | YYZ-YingSe/cordis-x | 1 | 可嵌入 Rust 插件运行时，多语言 SDK |
| 29 | mkroman/research_eu_downloader | 0 | 归档 EU CORDIS（科研数据库）杂志 PDF 的小工具 |
| 30 | bayunai/cordis-rs | 0 | 轻量进程内响应式运行时 |
| 31 | tr1v3r/cordis-rs | 0 | Cordis v4 插件运行时 Rust 版 |
| 32 | cunjif/nuomi | 1 | 基于 Cordis 的手绘风多 agent 桌面 |
| 33 | validation-engineering/cordis-verus | 0 | Verus 验证生命周期内核 + Cordis 兼容适配 |
| 34 | xuziqiang98/tiny-cordis | 0 | (无描述) |
| 35 | helloworldtang/cordis-rust | 1 | Cordis 插件框架 Rust 移植（deepseek-harness 底层） |
| 36 | collab-ai-dev/cordis-rs | 0 | (无描述) |
| 37 | Ricardo-M-L/cordis-di | 0 | 类型化插件与 DI 框架，受 Cordis 启发 |
| 38 | wwog/cordis_wasm | 1 | 以 Wasmtime Component Model 为插件机制的 Rust 运行时 |
| 39 | enginefuture/cordis-mobile-bridge | 0 | Tauri 2 + Rust Android 桥接 |
| 40 | harry-ma-qiang/tenon | 1 | DeepSeek Harness (Cordis) 微内核 agent 运行时移植 |
| 41 | curtiseng/spatiotemporal | 4 | 时空可组合性演算的 Rust 实现 |
| 42 | possible055/agentshim | 4 | 面向 coding agent 的仓库工具与进程执行 |
| 43 | rachitdev/skillforge | 1 | 书转 skill 编译器 + DSH Cordis 插件 |
| 44 | ApatheticMioz/Castor | 10 | 本地 LLM 通用 agent 微内核 |
| 45 | AlexisDevos/dsh-remote-vps | 0 | DeepSeek Harness 远程 VPS 文件工具 |
| 46 | cilis/dsh-tauri-launcher | 2 | DeepSeek Harness (DSH) Launcher 插件 |
| 47 | metis-mind/metis | 0 | 自进化 agent 运行时（Luau 插件） |
| 48 | Cnkore007/dsh-Desktop-Client | 1 | DeepSeek Harness 桌面客户端 |
| 49 | youridol/dsh-launcher | 3 | deepseek-harness 启动器（Tauri 2 + Rust + React） |
| 50 | CyanoOrg/dsh-norm-spec | 0 | norm-spec 插件：会话级约定注入 |
| 51 | geminiai19811-jpg/dsh-desktop | 0 | DeepSeek Harness 桌面客户端 |

## 观察
- 命名模式：cordis-rs / cordis-rust / cordis / dsh-*（DeepSeek Harness 生态）
- 多数项目与 cordisjs/cordis（Koishi 生态的插件元框架）或 "DSH cordis"（DeepSeek Harness 内核）相关
- #29 mkroman/research_eu_downloader 与 EU CORDIS 科研数据库相关（语义不同，但确实匹配 cordis + Rust）
- 原始搜索 total_count=51，与返回条目数一致，无遗漏页

## Phase 5 调研（2026-10-20）：dougong 视角的相关性分类

### 锚点
deepseek-harness 在 `vendor/cordis/` vendored 了 **@deepseek-ai/cordis v4.0.5**（TypeScript 插件框架，
"Meta-Framework for Modern JavaScript Applications"，cordiverse/cordis 同源）：src 含 context/events/fiber/
logger/reflect/registry/service/utils，核心为 Context DI、Service、fiber 生命周期、类型化事件、scoped cleanup、
config loader/HMR。dougong（斗拱）= 该框架的 Rust 实现。

### 分类原则
内核实现/范式规格/同谱系移植留；纯消费端应用（launcher/客户端/TUI 应用）、DSH 生态插件（非内核）、
撞名无关项目（EU CORDIS、Discord、另一 Python CORDIS）删。存疑一律保留。

### DELETE（19 个，remote 已核对）
| # | 目录 | remote | 删除理由 |
|---|------|--------|----------|
| 1 | research_eu_downloader | mkroman/research_eu_downloader | EU CORDIS 科研数据库 PDF 下载器，完全撞名无关 |
| 2 | stevenke1981__cordis-rs | stevenke1981/cordis-rs | 另一谱系：Python "CORDIS 认知运行时"的 Rust 重写，非 cordis 插件框架 |
| 3 | ckhawks__cordis | ckhawks/cordis | Discord 风格聊天应用原型（自述不可用），仅同名 |
| 4 | HiruNya__cordis | HiruNya/cordis | 玩具级 Discord bot 框架，与 cordis 框架无关，仅同名 |
| 5 | CordisChat | d-vid04/CordisChat | 后量子加密群聊 Tauri 客户端，仅 Cordis 命名 |
| 6 | MikoLauncher | miko-p/MikoLauncher | Minecraft 启动器（应用），仅借用 Cordis 理念，非内核参考 |
| 7 | cordis-mobile-bridge | enginefuture/cordis-mobile-bridge | Android 社交应用控制桥，"Cordis Social"为另一产品 |
| 8 | leantoken | morluto/leantoken | agent 代码智能/上下文裁剪工具，与 cordis 内核无关 |
| 9 | agentshim | possible055/agentshim | coding agent 仓库工具/MCP server，仅提及 DSH 适配 |
| 10 | skillforge | rachitdev/skillforge | 书转 skill 编译器 + DSH 插件（消费端插件） |
| 11 | dsh-norm-spec | CyanoOrg/dsh-norm-spec | DSH 的 norm-spec 插件适配器（消费端插件） |
| 12 | dsh-remote-vps | AlexisDevos/dsh-remote-vps | DSH 的远程 VPS 工具插件（消费端插件） |
| 13 | rootkiller6788__dsh-launcher | rootkiller6788/dsh-launcher | DSH 桌面启动器/生态管理器（消费端应用） |
| 14 | youridol__dsh-launcher | youridol/dsh-launcher | DSH Windows 桌面管家（消费端应用） |
| 15 | dsh-tauri-launcher | cilis/dsh-tauri-launcher | DSH 的 Tauri 启动器 Web 插件（消费端） |
| 16 | dsh-Desktop-Client | Cnkore007/dsh-Desktop-Client | DSH 桌面客户端（消费端应用） |
| 17 | dsh-desktop | geminiai19811-jpg/dsh-desktop | DSH Windows 桌面聊天客户端（消费端应用） |
| 18 | dock | P0lar1ght/dock | 本地 Agent TUI 应用（使用 cordis 插件树，非内核实现） |
| 19 | Castor | ApatheticMioz/Castor | MCP 工具链/代理，与 cordis 内核无关 |

### KEEP（33 个 = 32 cordis 谱系 + deepseek-harness）
- 锚点：deepseek-harness（vendor/cordis TS 源码）
- DSH cordis 内核直接移植：redoop__cordis-rust、dsh-rs、helloworldtang__cordis-rust、dshbox__cordis-rs、
  cordisrs（aimbrace/cordisrs，内容与 dshbox__cordis-rs 相同，疑似镜像，标注为重复项）
- cordiverse/cordis v4 Rust 移植/重实现：eavae__cordis-rs、jincdream__cordis-rs（冻结 commit 行为兼容 + 163 场景
  测试映射）、tr1v3r__cordis-rs、y0usaf__cordis-rs、cordis_wasm（语义对齐 cordis@4.0.0-rc.9）、
  weibaohui__cordis-rs、collab-ai-dev__cordis-rs、bayunai__cordis-rs、seago__cordis-rs（论文定理落地）、
  wanwanhhh__cordis、zhouyunsheng31__cordis（fork+Rust 状态树内核+设计文档）、tiny-cordis（无 README 但为
  完整微内核实现：context/fiber/scope/unwind+测试）、cordis-di、cordis-x、cordis-verus（Verus 验证内核）、
  eric8810__rutis（五支柱）、arcships__rutis（★148 多语言插件运行时）、mloong-agent-kernel（cordis-core 工作区）、
  nucleus（明确自述 cordis 范式实现+WASI）、metis（fiber/registry/loader）
- 理论/规格：spatiotemporal（时空可组合性演算论文第5章 Rust 版）
- 基于内核的 harness（含内核实现/用法参考，borderline 保留）：ares、tessivum（独立 Rust harness 全移植）、
  localai、nuomi、tenon（Erlang/BEAM 内核，语言不同设计同源）、CordisClaw（实验性 CPN-net 插件树）

### 附注
- deepseek-harness README 称 DSH 97k⭐；其 python/ 目录还有 Python SDK，TS 为主
- 全部删除项均为 GitHub 公开仓库克隆，可随时按 remote 重新 clone 恢复

## 追加核查（2026-10-20）：aimbrace/cordisrs 镜像判定
- 判定：cordisrs（aimbrace/cordisrs）= dshbox/cordis-rs 的一次性快照副本，源头为 dshbox/cordis-rs
- 证据：cordisrs HEAD(d77cf919, 2026-10-05) 存在于 dshbox 历史；反向不存在；作者同为 dshbox-dev；
  aimbrace 库 2026-10-06 创建后仅 push 一次（40 秒内），0 star；dshbox 2026-08-15 创建持续更新（★59）；
  工作树差异仅 Cargo.lock 与 3 个 CI workflow（快照落后所致）；fork=false（手动整库上传，非 GitHub fork）
- 处理：按"保留源、删副本"规则删除 cordisrs；dshbox__cordis-rs 保留
- 参考库现状：32 仓库 = deepseek-harness + 31 个 cordis 谱系

## 官方论文 markdown 版获取（2026-10-20）
- 目标确认：DSH 官方 README 明确引用 arXiv:2608.25512《A Programming Paradigm for Spatiotemporal Composability》
  （Yifan Shi, Wei Zhang, Tianyi Cui — Peking University / DeepSeek-AI），即 DeepSeek 官方发表的 DSH 设计论文
- 排除项：HA0329 与 bluenets1 两个同名仓库仅含 PDF；cordiverse/paper（官方论文库）仅 README+LFS 指针；
  August06exe/cordis-heart 为研读笔记网页书（非论文转写）
- 命中：acryldev/cordis-meta-framework-spatiotemporal-composability 仓库中的
  paper-...-Peking University-DeepSeek-AI.md（全论文结构的 Markdown/LaTeX 重建：覆盖全部实质章节 + 公式(1)–(65)）
- 落盘：doc/research/A-Programming-Paradigm-for-Spatiotemporal-Composability.md
  （76,923 B，UTF-8，187 标题，229 公式块，2695 行；结构验证通过：0-4 章及后续小节与页码对照完整）
