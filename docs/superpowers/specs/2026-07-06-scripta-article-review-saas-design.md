# Scripta — AI 文章检测 / 修改 / 评分 SaaS 设计

> 工作代号 **Scripta**。建立在 harness-rs 之上的文章审阅系统：检测（写作质量 + AI 味）、
> 定点修改（段/句/词/字级）、评分，全程用真实 git 做版本控制，交付为 Web SaaS。

日期：2026-07-06
状态：设计已确认，待写实现计划

## 1. 目标与范围

**做什么**：用户可以**导入已有文章**，也可以**让 AI 从零起草一篇新文章**；随后得到两类
反馈——写作质量问题与 AI 味识别；可以选中任意段/句/词/字下指令让 AI 定点改写，也可以
接受全文扫描出的逐条建议；每一次接受的修改进入真实 git 历史，可对比、可回滚、可看评分
随版本的变化。

**本期范围（一期，交付完整 Web app）**：
- AI 起草：从主题/brief（可选大纲、风格、字数）生成初稿，落为文档首个版本。
- 双评分维度：写作质量 + AI 味（AIGC 判据）。
- 两种编辑流并存：全文扫描建议流 + 用户主动选中+下指令定点改写。
- 版本控制：底层真 git（gix），封装成产品化 UI（历史/diff/回滚），不暴露 git 命令。
- 前端 React + TipTap（ProseMirror 内核）；后端 Rust（harness）。
- SaaS 账号体系：个人账号 + 调用计量（为后续计费埋点）。

**明确不做（后续独立分期）**：团队/多租户/角色权限、支付/订阅/额度、实时协作。

**诚实边界**：AI 味是 LLM 按 rubric 判据打分，不是训练好的分类器，不承诺"检测器"级
准确率——UI 明确标注这一点。

## 2. 组件全景

```
React + TipTap 前端 (web/)
  编辑器 · 选区→改写 · 扫描建议浮层 · 评分卡 · 历史/diff UI
        │ HTTP + SSE（流式 token / 逐条 finding）
scripta-server (axum)
  账号/JWT · 文档路由 · 计量 · SSE 流 · 编排引擎调用
        ├── scripta-engine（harness AgentLoop + 3 工具 + 双评分 + planner/worker）
        ├── Postgres (sqlx)：users/documents/versions/findings/usage_events
        └── Git 存储（gix，每文档一 bare 库）：data/repos/<doc_id>.git
```

Scripta 是**独立项目**，位于 harness-rs 的上级目录 `../scripta`
（即 `/Users/liliang/Things/AI/base-rs/scripta`），自己的 git 仓库与 Cargo workspace，
通过 path 依赖引用 harness-rs 各 crate（`../harness/crates/...`），harness 发布后可切
crates.io 版本。目录结构：
- `scripta-engine/` — 纯 harness 逻辑，可脱离 web 用集成测试验证。
- `scripta-server/` — axum + 存储 + 计量。
- `web/` — 前端。
- `data/repos/<doc_id>.git` — 每文档 git 存储。

## 3. harness 引擎（scripta-engine）

一个 `AgentLoop` **Session** 承载稳定前缀缓存：稳定的评分 rubric + 工具 schema（已按名
排序，字节稳定）构成可缓存前缀；文档正文是易变部分，每次调用随请求发送。

**默认用多轮消息历史 prompt LLM（不做无状态单发）**：每篇文档绑定一个常驻 `Session`，
`scan` / `rewrite` / `score` 都作为该 Session 的 turn 追加进消息历史。模型因此天然携带
上下文——之前改过什么、用户拒绝过什么建议、语气/风格偏好——无需每次重新交代。三层上下文：
- **不变前缀**（system + rubric + 工具 schema）：字节稳定，吃满前缀缓存。
- **追加历史**（历次用户指令 + AI 改写摘要 + 接受/拒绝决定）：只保留指令与摘要，
  **不**把整篇正文重复塞进每一轮，避免 token 膨胀。
- **易变尾部**（当前最新正文片段/选区）：随最新 turn 发送。
- 历史增长治理：超过阈值时对早期 turn 做滚动摘要（保留决定与偏好，丢弃冗余正文）。
Session 与文档 git HEAD 对应；切换/回滚版本时重建 Session 种子历史。

暴露四个工具：

- **`draft_document`** — 入参 `{brief, outline?, style?, length?}`。默认两步：先产出大纲
  供用户增删/调整（可跳过），再据大纲扩写成初稿 MD。流式返回，落为文档首个 commit
  （作者 `ai:draft`）。跑 planner 强模型。起草完成后即进入统一的扫描/改写/评分闭环。
- **`scan_document`** — 读当前 MD，返回结构化 findings：
  `{dimension, block_hash, char_start, char_end, severity, message, suggested_fix}`。
  跑两套 rubric：
  - 写作质量：逻辑、结构、论证、可读性、语法、事实性。
  - AI 味：陈词滥调、句式规整度、过渡词密度、模板化开头结尾等 LLM 判据信号。
- **`rewrite_span`** — 入参 `{block_hash, char_start, char_end, instruction}`，
  **只**返回该区间的替换文本，不落库（先给前端出 diff，接受后才 commit）。
- **`score_document`** — 返回两张评分卡：
  `quality{logic,structure,argument,readability,grammar,factuality,overall}` +
  `ai_likeness{probability, signals[]}`。

**模型路由**：planner（强模型）跑 `scan`/`score`/全文推理；worker（flash）跑单区间
`rewrite`/换词。用 harness `DynModel` + `Session`。模型一律 `base_url + model + key + type`
可配，不硬编码任何厂商 URL（遵循项目既有 model-provider 约定）。

## 4. Git + 文档模型（方案 A：Markdown 存 git 为唯一真相）

- 每文档一个 bare 仓库，正文 blob = `article.md`，主分支 `main`。用 **gix**（纯 Rust）
  读写 blob + commit + diff；若个别 API 有缺口，退回 `git2`。
- **一次接受的修改 = 一次 commit**，作者标记 `user` 或 `ai:<instruction>`，
  commit message 记录改动摘要。
- **AI 改写变体走分支**（`ai/rewrite-N`）；用户选中某变体 → fast-forward 合入 `main`。
- **锚点索引**：每个版本解析出块索引 `block_hash → char_span`，作为 git 跟踪的
  `index.json` 存进同一 commit。扫描建议引用 `block_hash`；文档变化后按 hash 重新解析，
  失败则模糊匹配兜底。锚点思路复用 harness hashline（内容哈希行锚定）。
- **产品化 UI（不暴露 git 命令）**：历史时间线（= git log）、双版本 diff（git word-diff
  展示到词级）、回滚（生成 revert commit）、评分随版本走势图。

### 选区→改写流程（精确）

1. 前端：TipTap 选中 → 把 ProseMirror pos 映射到 MD 字符偏移（维护渲染时 pos↔offset 映射）。
2. `POST /documents/:id/rewrite {char_start, char_end, instruction}`。
3. 后端加载当前 MD blob，取出该区间，走 worker 模型经 `rewrite_span` 得到替换文本，
   流式返回 diff（**不落库**）。
4. 前端展示 inline diff；接受 → `POST /documents/:id/commit` → 拼回 + commit →
   返回新版本 id + 更新后的块索引。
5. 拒绝 → 丢弃。

### 扫描流程

1. `POST /documents/:id/scan` → 跑 `scan_document`（planner）→ SSE 逐条推 finding。
2. 前端用 ProseMirror **decoration** 按 `block_hash` 锚点着色标注；
   用户看 message + suggested_fix → accept（走 commit 路径）/ dismiss。

### 评分流程

- `POST /documents/:id/score` → 返回两张评分卡，存进该版本；历史面板画评分走势。

## 5. 后端 API（scripta-server, axum）

```
POST   /auth/register            {email, password}
POST   /auth/login               → JWT cookie
GET    /documents                列出我的文章
POST   /documents                新建：{mode: import, text} 导入已有正文
                                 或 {mode: blank} 空白起步（首个空 commit）
POST   /documents/:id/draft      {brief, outline?, style?, length?} → SSE：大纲 → 初稿
                                 → 落为首个 ai:draft commit
GET    /documents/:id            当前 MD + 锚点索引 + HEAD
POST   /documents/:id/scan       → SSE 流：逐条 finding
POST   /documents/:id/rewrite    {char_start,char_end,instruction} → 流式替换文本 + diff（不落库）
POST   /documents/:id/commit     {edit|finding_id, accept} → 拼回 + commit → 新版本
POST   /documents/:id/score      → 双评分卡（存进该版本）
GET    /documents/:id/history    版本时间线（git log + 缓存评分）
GET    /documents/:id/diff       {from_sha,to_sha} → 词级 diff
POST   /documents/:id/revert     {sha} → revert commit
GET    /usage                    我的调用计量
```

- 流式用 **SSE**（token 与 finding 逐条推）。
- 鉴权 JWT cookie，密码 argon2。
- 端口用 3000 以上的随机端口（遵循项目约定）。

## 6. 数据模型（Postgres / sqlx）

git 存正文，Postgres 存元数据 + 缓存 + 计量（`versions` 镜像 git log，避免每次翻仓库）：

```
users(id, email, password_hash, created_at)
documents(id, user_id, title, repo_path, head_sha, created_at, updated_at)
versions(id, document_id, commit_sha, parent_sha, author_kind,
         message, quality_score jsonb, ai_score jsonb, created_at)
findings(id, document_id, version_sha, dimension, block_hash,
         char_start, char_end, severity, message, suggested_fix,
         status)                         -- open | accepted | dismissed
usage_events(id, user_id, document_id, kind, model,
             input_tokens, cached_tokens, output_tokens, created_at)
```

## 7. 前端（React + TipTap / ProseMirror）

- **新建入口**：两条路径——导入/粘贴已有文章，或 AI 起草（填主题/brief + 可选风格、字数）。
  起草时先展示 AI 大纲供增删调整（可跳过），再流式扩写成初稿注入编辑器。
- **选区→改写**：选中 → pos↔offset 映射 → inline 指令框 → `/rewrite` 流式 diff 浮层 →
  接受 `/commit`。
- **扫描建议**：findings 用 decoration 按 `block_hash` 锚点着色；点开看 message +
  suggested_fix；accept / dismiss。
- **评分卡**：侧栏两张卡（质量雷达 + AI 味概率条 + 信号列表）；历史面板画评分走势。
- **历史/diff**：时间线选两版本 → 词级 diff → 一键回滚。

## 8. 计量（为计费埋点）

每次 `draft/scan/rewrite/score` 落一条 `usage_events`，记 input/cached/output tokens
（harness `Outcome.usage` 直接提供，含前缀缓存命中数）。`/usage` 出个人用量看板。
本期只计量、不计费。

## 9. 分期建设顺序（交付物仍是完整 Web app）

- **P0 引擎+git**：`scripta-engine` + gix 存储 + 文档/锚点模型，用集成测试对真实模型验证
  draft / scan / rewrite / score / commit / diff 闭环（不碰前端）。
- **P1 后端**：axum API + 账号/JWT + Postgres + 计量 + SSE。
- **P2 前端**：TipTap 编辑器、选区改写、扫描 decoration、评分卡、历史/diff。
- **P3 打磨**：用量看板、错误态、体验。

## 10. 待实现计划阶段再定的细节

- gix vs git2 的最终取舍（先按 gix，遇 API 缺口切 git2）。
- rubric 的具体 prompt 与 findings JSON schema 定稿。
- pos↔offset 映射在 TipTap 侧的具体实现（自定义扩展）。
- SSE 事件协议（token / finding / done / error 的帧格式）。
