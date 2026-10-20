# Task Plan: dougong（斗拱）实现研究

## Goal
研究论文、TS 版 cordis、Rust 参照项目，产出 dougong 实现研究：内核语义映射 + dylib/so 与 wasm 双插件形态设计输入 + 开放问题清单。研究结果实时保存 doc/research/study-*.md。

## 硬约束（用户给定）
- dougong = cordis Rust 实现，通用基座；llmharness = 其上插件集；插件仅 dylib/so + wasm
- 存疑必确认；WASM/WIT 以技能+最新文档为准；引用注明 commit/版本

## Next Step
等待用户答复 R1-R3（dylib 镜像常驻可接受性、目标平台/MSRV、crate 命名）；裁决后另立实现计划并开始编码。

## Current Phase
Phase 6

## Phases

### Phase 1: 论文精读
- [x] 2695 行全文 → study-1-paper.md
- **Status:** complete

### Phase 2: TS cordis 源码精读
- [x] 8 模块（v4.0.5 @ 687f282e86）→ study-2-ts-cordis.md
- **Status:** complete

### Phase 3: Rust 参照项目对比
- [x] 13 项目档案（redoop 深读；jincdream/cordis-x/cordis_wasm/nucleus 中读；6 项轻读）→ study-3
- **Status:** complete

### Phase 4: 插件边界技术输入
- [x] Skill(wasmtime)/Skill(wit) 已调用；→ study-4-plugin-boundary.md
- **Status:** complete

### Phase 5: 设计草案 + 开放问题
- [x] doc/research/dougong-design.md（语义清单/工作区草案/14 开放问题含推荐）
- **Status:** complete

### Phase 6: 与用户确认
- [ ] 用户逐项裁决 14 项开放问题；裁决写入 findings
- [ ] 后续：按裁决另立实现计划（未确认不编码）
- **Status:** in_progress

## Decisions Made
| Decision | Reason |
|----------|--------|
| 研究产出=报告+决策清单，实现另起 | 用户仅授权"研究如何实现" |
| 语义基线=论文清单 + jincdream 163 场景矩阵 + 论文 §15 验收模板 | 最完备的可验证对齐来源 |
| WIT 单一真源同时服务 wasm/dylib | 一致性与工程量平衡（用户确认 Q7 后细化） |
| 全部 WASM/WIT 结论标注来源 | 用户明示模型记忆过时 |

## Errors Encountered
| Error | Attempt | Resolution |
|-------|---------|------------|
| init 落错目录 | 1 | 迁移至项目根并 set-active-plan |
| wasm/wit 技能初次缺失 | 1 | /reload-skills 后可用 |
