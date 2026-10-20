# Task Plan: 维护 dougong 的 cordis 参考库

## Goal
维护 `~/project/reference/cordis/` 参考库与 doc/research 资料：围绕项目 dougong（斗拱）——参考 deepseek-harness 中所用 cordis（vendor/cordis 的 @deepseek-ai/cordis v4.0.5 TS 插件框架）实现 Rust 版——只保留有参考价值的仓库，删除无关/重复项目，并收集官方研究资料（谨慎，存疑保留）。

## Next Step
任务已完成，无下一步。

## Current Phase
全部完成

## Phases

### Phase 1: GitHub 搜索 cordis 相关 Rust 项目
- [x] GitHub Search API `cordis language:Rust`，51 项全取
- **Status:** complete

### Phase 2: 过滤与确认清单
- [x] 51 项全部 fork=false、language=Rust
- **Status:** complete

### Phase 3: 批量 clone 到目标目录
- [x] 51 个全部克隆成功（同名冲突组用 `所有者__仓库名`）+ deepseek-harness（共 52 目录）
- **Status:** complete

### Phase 4: 验证
- [x] 52 目录均含 .git；Cargo.toml 全覆盖；remote URL 与清单 diff 完全一致
- **Status:** complete

### Phase 5: 相关性调研（dougong 视角）
- [x] 锚点：DSH 的 cordis = vendor/cordis 内 @deepseek-ai/cordis v4.0.5（TS）
- [x] 分类表写入 findings.md：DELETE 19 / KEEP 33
- **Status:** complete

### Phase 6: 谨慎删除
- [x] 删除 19 个明确无关仓库（删前 remote 逐一核对）；borderline 全部保留
- **Status:** complete

### Phase 7: 验证与收尾
- [x] 33 目录与 KEEP 清单一致；remote 完好；810M
- **Status:** complete

### Phase 8: dsh-launcher 与 cordisrs 镜像核查
- [x] dsh-launcher ×2：非互为 fork（fork=false 无 parent），独立同名项目；已删状态维持
- [x] cordisrs（aimbrace）：确认为 dshbox/cordis-rs 一次性快照副本 → 删副本留源头
- [x] 终验：32 目录
- **Status:** complete

### Phase 9: 官方论文 markdown 版获取
- [x] 锚定官方论文：arXiv:2608.25512《A Programming Paradigm for Spatiotemporal Composability》（Shi/Zhang/Cui，Peking University / DeepSeek-AI）
- [x] 排除仅-PDF 候选（HA0329、bluenets1、cordiverse/paper 官方库）与研读笔记类（cordis-heart）
- [x] 命中 acryldev 文档仓库的论文 Markdown/LaTeX 全文重建版并下载
- [x] 验证署名/结构/公式完整性，落盘 `doc/research/A-Programming-Paradigm-for-Spatiotemporal-Composability.md`
- **Status:** complete

## Decisions Made
| Decision | Reason |
|----------|--------|
| 锚点 = deepseek-harness vendor/cordis 的 @deepseek-ai/cordis v4.0.5 | 用户目标：实现其 Rust 版 dougong |
| 分类原则：内核实现/范式规格/同谱系移植留；消费端应用/DSH 生态插件/撞名项目删 | 参考价值来自内核与范式 |
| 存疑一律保留 | 用户强调"小心点不要乱删" |
| 重复副本：保留源头、删除副本（须 git 对象级/API 证据） | 用户规则"保留 fork 源删除 fork 项目" |
| 论文仅收 DeepSeek 官方（arXiv:2608.25512），署名含 DeepSeek-AI | 用户明确"其他机构发表的不要" |
| 删除安全性 | 全部为 GitHub 公开仓库克隆，可按 remote 重新 clone 恢复 |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| 目标路径拼写错误（projetc/rferenc） | 1 | 用户更正；确认空目录后删除重建 |
| 15 个同名仓库被 SKIP | 1 | 冲突组 `所有者__仓库名` 目录名，补克隆后验证 0 缺失 |
| 保留清单核对命令误混入 8 个已删名 | 1 | 重验通过（该 diff `<` 行恰为应缺项） |
