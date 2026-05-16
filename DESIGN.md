# harness — A Rust Agent Harness Framework

> **Agent = Model + Harness.**
> 这个仓库实现 *Harness* 那一半。Model 这一半交给已有的 LLM 客户端 crate
> (Rig, async-anthropic, async-openai, kalosm) 完成。

---

## 0. 设计依据

设计直接吸收以下公开材料的实证结论，不重新发明轮子：

- Lopopolo / OpenAI, *Harness engineering: leveraging Codex in an agent-first world* (2026-02)
- Böckeler / Thoughtworks, *Harness engineering for coding agent users* (2026-04)
- Stripe Engineering, *Minions: one-shot end-to-end coding agents* (Blueprint + Devbox + Toolshed)
- Anthropic Claude Code v2.1.88 逆向分析 (5-stage compaction, 27 hook events, 7 permission modes)
- `obra/superpowers` skills framework (SKILL.md, brainstorm → plan → TDD → subagent 流水线)

---

## 1. Mission

为 Rust 开发者提供**编译期类型安全的 harness 框架**，让一个 LLM
能在受控、可观测、可回放、可纠错的循环里完成真实工程任务。

口号：**Rig 给你 model + tool，harness 给你一切围着它们的控制层。**

### 我们要解决的核心问题

1. **Context 是稀缺资源**——内置渐进式压缩，而不是把所有文档塞进单一 prompt。
2. **行为可控**——前馈 (Guide) 与反馈 (Sensor) 双向闭环，而不是只靠 prompt 调教。
3. **确定性优先**——能用代码搞定的事 (lint / format / git / 移动文件) 不烧 token。
4. **隔离优于约束**——权限通过沙箱 (worktree / container / VM) 而不是运行时弹窗。
5. **可观测**——27 个 lifecycle 事件全开放，配合 `tracing` 全链路追踪。

---

## 2. Non-Goals

- ❌ 不做又一个 LLM 客户端 (用 Rig)。
- ❌ 不做又一个 vector store / RAG (用 swiftide / rs-agent)。
- ❌ 不绑定到单一模型供应商。
- ❌ 不做 GUI / IDE 集成（先做 CLI + 库）。
- ❌ 不优化分布式执行 (单机优先；多 agent 仅指逻辑上的子 agent)。

---

## 3. 概念地图

```
                    ┌────────────────────────────────────┐
                    │   Blueprint  (state machine)       │
                    │   ┌──────────┐    ┌──────────┐     │
                    │   │ Determ.  │───►│ Agent    │───► ...
                    │   │ Node     │    │ Node     │     │
                    │   └──────────┘    └────┬─────┘     │
                    └────────────────────────┼───────────┘
                                             │
                              ┌──────────────┴───────────────┐
                              │           AgentLoop          │
                              │                              │
        Guides ──────►        │  ctx ──► Model ──► Actions   │
        (feedforward)         │           ▲          │       │
                              │           │          ▼       │
        Sensors ◄──────────── │       Signals ◄── Tools      │
        (feedback)            │           ▲          │       │
                              │           │     World mut.   │
                              └───────────┼──────────────────┘
                                          │
                              ┌───────────┴───────────┐
                              │   HookBus (27 evts)   │
                              │   Compactor (5 stg.)  │
                              │   Sandbox (3 tiers)   │
                              └───────────────────────┘
```

### 三种核心控制

| 抽象        | 何时触发              | 执行方式                  | 例子                                |
| ----------- | --------------------- | ------------------------- | ----------------------------------- |
| **Guide**   | Model 看到 task 之前  | Computational / Inferential | AGENTS.md, skill, codemod, LSP 提示 |
| **Sensor**  | Action 执行之后        | Computational / Inferential | clippy, cargo test, review-agent    |
| **Hook**    | 27 个具体生命周期点    | 确定性回调                 | 写日志, 否决 tool, 注入上下文        |

---

## 4. Workspace 布局

```
harness/
├── Cargo.toml                     # [workspace] root
├── DESIGN.md                      # 本文件
├── README.md
├── crates/
│   ├── harness-core/              # ★ traits + types, 零外部依赖
│   ├── harness-macros/            # #[skill] #[tool] #[guide] #[sensor] proc-macro
│   ├── harness-context/           # Context, prompt builder, prompt cache
│   ├── harness-compactor/         # 5-stage compaction
│   ├── harness-hooks/             # HookBus + 27 个 Event 类型
│   ├── harness-loop/              # AgentLoop (ReAct + self-correct)
│   ├── harness-blueprint/         # state machine 编排
│   ├── harness-sandbox/           # Sandbox trait + worktree/container/vm
│   ├── harness-skills/            # SKILL.md & #[skill] 收集器, inventory 注册
│   ├── harness-tools-fs/          # 文件读写 / 编辑 / ripgrep
│   ├── harness-tools-shell/       # 受控 shell, 按 risk 分级
│   ├── harness-sensors-rust/      # cargo check/clippy/test/fmt/deny
│   ├── harness-sensors-common/    # git, lsp (rust-analyzer over LSP)
│   ├── harness-models/            # Model trait + Rig adapter, Anthropic, OpenAI
│   ├── harness-templates/         # 预置 Blueprint: crate-keeper, axum-crud, ...
│   └── harness-cli/               # `harness run`, `harness diff`, ...
├── examples/
│   ├── crate-keeper/              # MVP demo: 自动维护一个 Rust crate
│   └── axum-crud-minion/
└── xtask/                         # build/release 辅助
```

依赖图（顶层 → 底层）：

```
cli  ──► templates ──► blueprint ──► loop ──► context + hooks + compactor + sandbox + skills
                                       │                              │
                                       ▼                              ▼
                                     core ◄────── macros, sensors, tools, models
```

**`harness-core` 不依赖任何非 std + 少量必需 crate (serde, thiserror, async-trait)**，方便所有上层 crate 共享同一组 trait 定义。

---

## 5. `harness-core` — 核心 trait

```rust
// crates/harness-core/src/model.rs
#[async_trait]
pub trait Model: Send + Sync + 'static {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError>;
    async fn stream(&self, ctx: &Context) -> Result<BoxStream<'_, ModelDelta>, ModelError>;
    fn info(&self) -> ModelInfo;          // 上下文窗口、价格、能力
}

// crates/harness-core/src/tool.rs
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn schema(&self) -> &ToolSchema;
    fn risk(&self) -> ToolRisk;            // ReadOnly | Idempotent | Destructive | Network
    async fn invoke(&self, args: Value, w: &mut World) -> Result<ToolResult, ToolError>;
}

// crates/harness-core/src/guide.rs
#[async_trait]
pub trait Guide: Send + Sync + 'static {
    fn id(&self) -> GuideId;
    fn kind(&self) -> Execution;           // Computational | Inferential
    fn scope(&self) -> GuideScope;
    async fn apply(&self, ctx: &mut Context, w: &World) -> Result<(), GuideError>;
}

// crates/harness-core/src/sensor.rs
#[async_trait]
pub trait Sensor: Send + Sync + 'static {
    fn id(&self) -> SensorId;
    fn kind(&self) -> Execution;
    fn stage(&self) -> Stage;              // PreAction | SelfCorrect | PreCommit | PostIntegrate | Continuous
    async fn observe(&self, action: &Action, w: &World) -> Result<Vec<Signal>, SensorError>;
}

// crates/harness-core/src/signal.rs — ★ 反馈信号必须对 LLM 友好
pub struct Signal {
    pub severity:   Severity,              // Block | Warn | Hint
    pub origin:     SensorId,
    pub message:    String,                // human-readable
    pub agent_hint: Option<String>,        // LLM 纠错指令 (必填 if Block)
    pub auto_fix:   Option<FixPatch>,      // 可直接打补丁, 跳过模型
    pub location:   Option<CodeSpan>,
}

// crates/harness-core/src/compactor.rs
#[async_trait]
pub trait Compactor: Send + Sync + 'static {
    fn budget(&self, ctx: &Context) -> Budget;
    async fn compact(&self, stage: CompactionStage, ctx: &mut Context) -> Result<(), CompactError>;
}

// crates/harness-core/src/hook.rs
pub trait Hook: Send + Sync + 'static {
    fn matches(&self, ev: &Event) -> bool;
    fn fire(&self, ev: &Event, w: &mut World) -> HookOutcome;
}

pub enum HookOutcome {
    Allow,
    Deny { reason: String },
    Mutate(EventPatch),
    Inject(ContextPatch),
}
```

`Context`、`World`、`Action`、`Event` 等类型也定义在 `harness-core`：

```rust
pub struct Context {
    pub system:     Vec<Block>,            // prompt cache 友好: 长前缀稳定
    pub guides:     Vec<Block>,
    pub history:    Vec<Turn>,
    pub task:       Task,
    pub budget:     Budget,
    pub metadata:   serde_json::Value,
}

pub struct World {
    pub repo:       RepoView,              // git + filesystem
    pub runner:     Arc<dyn ProcessRunner>,
    pub clock:      Arc<dyn Clock>,
    pub kv:         Arc<dyn KvStore>,      // skills 之间的小型共享
}
```

---

## 6. Skills — 严格对齐 [agentskills.io](https://agentskills.io/specification)

### 6.0 关键决定

Skill **必须**符合 Agent Skills 公开规范：

- Skill = 目录 (`<name>/SKILL.md` + 可选 `scripts/` / `references/` / `assets/`)
- frontmatter 只允许 6 个字段：`name` `description` (必填) + `license` `compatibility` `metadata` `allowed-tools` (可选)
- **激活由 agent 通过 `description` 决定**——框架不再硬编码 `triggers`
- 渐进披露三档：metadata (~100 tokens, 总是加载) → body (\<5000 tokens, 激活时) → resources (按需)
- `name` 必须匹配目录名、小写字母数字 + 连字符、不能以连字符开头或结尾、不能连续连字符

→ 这意味着 **harness 中的 skill 直接复用 obra/superpowers、Anthropic skills 市场等任何兼容仓库**，零适配成本。

### 6.1 框架专属扩展走 `metadata.harness.*` 命名空间

规范在 `metadata` 字段上明确允许自定义键值，并推荐"key 名独特化以免冲突"。我们的扩展全部放进 `metadata.harness.*` 子树，保持 spec 兼容：

```yaml
---
name: format-rust
description: Run cargo fmt across the workspace. Use after edits to .rs files or before committing Rust code.
license: Apache-2.0
allowed-tools: Bash(cargo:fmt) Read
metadata:
  harness:
    kind: computational           # computational | inferential
    risk: read-only               # read-only | idempotent | destructive | network
    entrypoint: "skill::run"      # Rust 函数引用 (仅 #[skill] 宏生成的 skill 有)
    schema-version: "1"
---
```

未识别这些键的 agent (e.g. Claude Code) 会**安全忽略**——skill 在他们眼里只是普通 SKILL.md，照常工作。

### 6.2 三种加载方式

| 形式 | 命令 | 用途 |
|------|------|------|
| **A. 目录加载** | `harness::skills_dir!("./skills")` | 加载第三方 / 手写的 SKILL.md 目录树 |
| **B. `#[skill]` 宏 (函数)** | 见下文 | 行为是确定性 Rust 代码的 skill |
| **C. `#[skill]` 宏 (结构体)** | 见下文 | 需要状态/依赖注入的 skill |

三种形式产出同一种运行时类型 `SkillHandle`。

### 6.3 形式 A：目录加载（最常用）

```rust
// 编译期递归扫描, 解析 SKILL.md, 校验 frontmatter, 提交到 inventory
harness::skills_dir!("./skills");
```

宏在编译期完成 `skills-ref validate` 的等价检查；违反规范则**编译失败**：

- name 字段缺失 / 格式非法 → compile_error!
- name 不匹配目录名 → compile_error!
- description 超过 1024 字符 → compile_error!
- frontmatter 出现未知顶层字段 → warning（spec 允许 metadata，但不允许顶层未知字段）

### 6.4 形式 B：`#[skill]` 函数

```rust
use harness::prelude::*;

/// Run cargo fmt across the workspace. Use after edits to .rs files or
/// before committing Rust code.
#[skill(
    name        = "format-rust",
    license     = "Apache-2.0",
    allowed_tools = "Bash(cargo:fmt)",
    harness(kind = "computational", risk = "read-only"),
)]
async fn format_rust(ctx: &mut Context, w: &mut World) -> Result<()> {
    w.runner.exec("cargo fmt --all").await?;
    Ok(())
}
```

**description 字段从 doc-comment 自动取**（如果有 `description=` 参数则优先）。
这种 skill 在构建时被宏物化为一个真实的 SKILL.md：

```
target/harness/skills/format-rust/
└── SKILL.md          # 自动生成, 内容由宏渲染
```

这样**外部 agent 也能消费**——`cargo build` 之后这个目录就是 spec 合规的 skill 包。

### 6.5 形式 C：`#[skill]` 结构体

```rust
#[skill(name = "review-axum-handlers")]
pub struct AxumReview {
    pub model: Arc<dyn Model>,
    pub rules: PathBuf,
}

impl SkillBody for AxumReview {
    fn description(&self) -> &'static str {
        "Review axum HTTP handlers for security, error handling, and tracing. \
         Use when reviewing or writing code in src/api/**."
    }

    fn body(&self) -> Cow<'_, str> {
        // 运行时渲染 SKILL.md 主体（可以读 self.rules）
    }

    async fn run(&self, ctx: &mut Context, w: &mut World) -> Result<()> {
        // 可选: 如果 skill 同时也是动作执行者
    }
}
```

### 6.6 `Skill` trait

```rust
// crates/harness-core/src/skill.rs
pub trait Skill: Send + Sync + 'static {
    fn manifest(&self) -> &SkillManifest;     // spec 字段 (name, description, ...)
    fn body(&self) -> Cow<'_, str>;            // Markdown body
    fn resources(&self) -> &[Resource];        // scripts/references/assets 索引
    fn handler(&self) -> Option<SkillHandler>; // 仅 #[skill] 宏生成的 skill 有
}

pub struct SkillManifest {
    pub name:          String,
    pub description:   String,
    pub license:       Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Vec<ToolPattern>,       // 解析后的 Bash(git:*) 等
    pub metadata:      BTreeMap<String, Value>,
}

impl SkillManifest {
    pub fn harness_ext(&self) -> Option<&HarnessExt>; // 读 metadata.harness.* 子树
}
```

### 6.7 Skill 激活流程（spec 兼容）

```
┌─ session start ─────────────────────────────────────────────┐
│ 1. 收集所有 skill 的 (name, description)                     │
│ 2. 把它们渲染成一段 system prompt:                            │
│      "Available skills:\n- format-rust: ...\n- ..."         │
│ 3. agent 看见 description 后, 通过 tool call `activate_skill │
│    {name}` 决定激活                                          │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─ on activate_skill ─────────────────────────────────────────┐
│ 1. 加载 SKILL.md 完整 body 进 context                        │
│ 2. 若 skill 有 handler (形式 B/C), 立即调用一次              │
│ 3. 把 resources/ 索引也加进 context (path + 1 行摘要)        │
└──────────────────────────────────────────────────────────────┘
```

`activate_skill` 本身就是一个内置 `Tool`，因此整个机制不需要任何超出 spec 的扩展。

### 6.8 与 Hook Handler 的边界（重要）

> ⚠️ 之前草稿里把"在 PreToolUse 时自动跑 fmt"也叫 skill —— **错了**。
> 那种东西 spec 不管，应该叫 **Hook Handler**。

| 概念 | 触发者 | 何时跑 | 用途 |
|------|--------|--------|------|
| **Skill** | LLM 自己 (根据 description) | 模型判断需要时 | 知识、流程、可选动作 |
| **Hook Handler** | HookBus (生命周期事件) | 确定性事件触发 | 自动 fmt、否决 destructive、注入压缩提醒 |

→ skill 走 spec，**hook handler 走单独的 `#[hook]` 宏**。

```rust
#[hook(event = "PostToolUse", matches = "tool = 'edit_file' && path ~ '*.rs'")]
async fn auto_fmt_on_edit(ev: &Event, w: &mut World) -> HookOutcome {
    w.runner.exec("cargo fmt --all").await.ok();
    HookOutcome::Allow
}
```

### 6.9 其他三个宏

`#[tool]` / `#[guide]` / `#[sensor]` 不在 agentskills.io 规范范围内，是 harness 框架自有概念，规则更宽松：

```rust
#[tool(name = "ripgrep", risk = "read-only")]
async fn ripgrep(args: RgArgs, w: &mut World) -> Result<RgResult> { ... }

#[guide(scope = "files:src/api/**")]
async fn api_conventions(ctx: &mut Context, w: &World) -> Result<()> { ... }

#[sensor(stage = "self-correct", kind = "computational")]
async fn clippy_sensor(action: &Action, w: &World) -> Result<Vec<Signal>> { ... }
```

四个宏 (`#[skill]` `#[tool]` `#[guide]` `#[sensor]`) + 一个 `#[hook]` 共享 `harness-macros` 内部基础设施 (`inventory` 注册、frontmatter 渲染、编译期校验)。

### 6.10 校验工具

`harness-cli` 内嵌 `skills-ref` 兼容的校验：

```bash
harness skills validate ./skills/my-skill
harness skills validate --all
harness skills export target/harness/skills    # 把 #[skill] 宏生成的目录导出
```

`harness skills export` 输出的目录可直接被 Claude Code / Cursor / Codex 等任意 spec 兼容 agent 消费——这是我们的**可移植性承诺**。

---

## 7. AgentLoop — 自纠正主循环

```rust
// crates/harness-loop/src/lib.rs
pub async fn run<M: Model>(
    model:     &M,
    tools:     &ToolRegistry,
    guides:    &[Arc<dyn Guide>],
    sensors:   &SensorBus,
    compactor: &dyn Compactor,
    hooks:     &HookBus,
    mut ctx:   Context,
    mut world: World,
    policy:    Policy,
) -> Outcome {
    hooks.fire(Event::SessionStart { source: ctx.source() }, &mut world);

    // 1. 注入所有匹配的 guide
    for g in guides.iter().filter(|g| g.scope().matches(&ctx.task)) {
        g.apply(&mut ctx, &world).await?;
    }

    // 2. ReAct loop
    for iter in 0..policy.max_iters {
        // 2a. 预算检查 → 必要时压缩
        for stage in compactor.budget(&ctx).required_stages() {
            hooks.fire(Event::PreCompact { stage }, &mut world);
            compactor.compact(stage, &mut ctx).await?;
            hooks.fire(Event::PostCompact { stage }, &mut world);
        }

        // 2b. 调模型
        let out = model.complete(&ctx).await?;
        ctx.history.push(out.as_turn());

        let actions = out.into_actions();
        if actions.is_empty() { return Outcome::Done(ctx); }

        // 2c. 执行 actions, 每个都经过 hook + sensor 闭环
        for action in actions {
            if let HookOutcome::Deny { reason } =
                hooks.fire(Event::PreToolUse { action: &action }, &mut world)
            {
                ctx.append_denial(action, reason);
                continue;
            }

            let result = tools.dispatch(&action, &mut world).await;
            hooks.fire(Event::PostToolUse { action: &action, result: &result }, &mut world);

            let signals = sensors.run(Stage::SelfCorrect, &action, &world).await;
            let (auto, remaining) = signals.partition_auto_fix();
            world.apply_patches(auto);                                   // 计算化修复直接落盘

            if remaining.has_blocking() {
                ctx.append_feedback(remaining);                           // 反馈喂回模型
            }
        }
    }
    Outcome::BudgetExhausted(ctx)
}
```

要点：

- **Sensor 在每个 action 之后跑**——这是文章里 "feedforward + feedback" 闭环的对应实现。
- **`auto_fix` 通道**——`cargo fmt`、`clippy --fix` 这类计算化修复跳过模型直接打补丁，省 token、避免模型把简单修复搞复杂。
- **`PreCompact` / `PostCompact` 事件**——填补官方 Claude Code 目前没有的事件，方便 skills 在压缩前后保护"身份文档"。

---

## 8. Blueprint — 确定性 + agent 混合状态机

借鉴 Stripe Minions，**不是所有事都让 agent loop 处理**：

```rust
// crates/harness-blueprint/src/lib.rs
pub enum Node {
    Deterministic(Arc<dyn Fn(&mut World) -> BoxFuture<'_, Result<Transition>>>),
    Agent {
        guides:  Vec<Arc<dyn Guide>>,
        tools:   ToolRegistry,
        sensors: SensorBus,
        budget:  AgentBudget,
    },
    Subagent(SubagentSpec),                // 隔离 context + worktree
    Branch(Arc<dyn Fn(&World) -> NodeId>),
    Parallel(Vec<NodeId>),
}

pub struct Blueprint {
    nodes:  HashMap<NodeId, Node>,
    edges:  Edges,
    start:  NodeId,
    sandbox: Box<dyn Sandbox>,
}

impl Blueprint {
    pub async fn run(self, task: Task) -> Outcome { /* 状态机调度 */ }
}
```

**示例**：crate-keeper minion

```rust
Blueprint::new(WorktreeSandbox::new())
    .add("read_changelog", Node::deterministic(|w| async move {
        w.runner.exec("git log --oneline -n 50").await
    }))
    .add("bump_deps",      Node::Agent { guides: deps_guides(),  tools: edit_tools(), .. })
    .add("fmt",            Node::deterministic(|w| w.runner.exec("cargo fmt --all")))
    .add("clippy_fix",     Node::deterministic(|w| w.runner.exec("cargo clippy --fix --allow-dirty")))
    .add("test",           Node::deterministic(|w| w.runner.exec("cargo nextest run")))
    .add("write_changelog",Node::Agent { .. })
    .add("review",         Node::Subagent(SubagentSpec::review_only()))
    .edge("read_changelog", "bump_deps")
    .edge("bump_deps", "fmt").edge("fmt", "clippy_fix").edge("clippy_fix", "test")
    .branch_on_failure("test", retry_cap = 2)
    .edge("test", "write_changelog").edge("write_changelog", "review")
```

---

## 9. Compactor — 5 阶段渐进式压缩

完全照搬 Claude Code 的实证设计：

| 阶段                 | 阈值                  | 做什么                                          |
| -------------------- | --------------------- | ----------------------------------------------- |
| 1. `BudgetReduce`    | > 60% 窗口            | 截掉冗余 tool result，保留最近 N 个完整 turn     |
| 2. `Snip`            | > 70% 窗口            | 删除老旧 file read，留 path + hash               |
| 3. `Microcompact`    | > 80% 窗口            | 用 cheap model (Haiku) 摘要老对话段              |
| 4. `ContextCollapse` | > 90% 窗口            | 把所有 file read 合并成一个文件清单 + 关键摘录   |
| 5. `AutoCompact`     | > 95% 窗口            | 整段对话用 main model 重写为压缩形式             |

阶段是**累积**的：高一档触发时，先跑低档全部。每阶段都发出 `PreCompact` / `PostCompact` 事件以便 hook 注入"必须保留"的段落（例如 AGENTS.md 的身份指令）。

---

## 10. HookBus — 27 个事件

```rust
pub enum Event<'a> {
    SessionStart { source: SessionSource },                     // startup|resume|clear|compact
    SessionEnd,
    PreToolUse  { action: &'a Action },
    PostToolUse { action: &'a Action, result: &'a ToolResult },
    PermissionRequest { action: &'a Action },
    PreCompact  { stage: CompactionStage },
    PostCompact { stage: CompactionStage },
    PreGuide    { guide: GuideId },
    PostGuide   { guide: GuideId },
    PreSensor   { sensor: SensorId },
    PostSensor  { sensor: SensorId, signals: &'a [Signal] },
    PreModel    { ctx: &'a Context },
    PostModel   { out: &'a ModelOutput },
    SubagentStart  { spec: &'a SubagentSpec },
    SubagentReport { status: SubagentStatus },                  // DONE|DONE_WITH_CONCERNS|BLOCKED|NEEDS_CONTEXT
    FileChanged    { path: &'a Path },
    CwdChanged     { from: &'a Path, to: &'a Path },
    BlueprintNodeEnter { node: NodeId },
    BlueprintNodeExit  { node: NodeId, transition: &'a Transition },
    TaskCompleted,
    BudgetWarning  { ratio: f32 },
    Notification   { kind: NotificationKind },
    Error          { err: &'a HarnessError },
    Stop,
    Heartbeat      { iter: u32 },
    Custom         { name: &'a str, data: &'a Value },
}
```

Hook 实现可以是 native Rust 函数、shell 脚本（兼容 Claude Code hooks 格式）、或者 MCP 工具调用。

---

## 11. Sandbox — 三层隔离

```rust
pub trait Sandbox: Send + Sync {
    async fn spawn(&self, plan: &Blueprint) -> Result<SandboxHandle, SandboxError>;
    fn fs_policy(&self) -> FsPolicy;
    fn net_policy(&self) -> NetPolicy;
}

pub struct WorktreeSandbox  { /* git worktree + ro 工具白名单 */ }
pub struct ContainerSandbox { image: String, net: NetPolicy /* OCI 容器, 默认无网络 */ }
pub struct VmSandbox        { /* Firecracker microVM, 类 Stripe Devbox */ }
```

**核心原则**：权限不是运行时弹窗，而是 spawn 时一次性烧进沙箱。`Tool::risk()` 用于沙箱选择策略，而不是用于在每个 tool call 时打断用户。

> 例外：当用户运行交互模式 (`harness run -i`) 时，hook 仍可生成 `PermissionRequest` 走 CLI 提示——但这是 hook 的实现细节，不是核心模型。

---

## 12. 扩展点对照表

五种扩展机制按上下文成本递增：

| 机制       | 上下文成本    | 谁触发                     | 例子                                              |
| ---------- | ------------- | -------------------------- | ------------------------------------------------- |
| **Hook**   | 几乎为零      | HookBus (27 事件, 确定性)  | 写日志、否决 destructive tool、auto-fmt           |
| **Guide**  | 启动一次性    | 框架按 task scope 自动注入 | AGENTS.md、API 约定、reference architecture       |
| **Sensor** | 行动后        | 框架按 stage 触发           | clippy、cargo test、review agent                  |
| **Skill**  | ~100 tok/个 + 按需 | **模型自己**根据 description 决定激活 | "如何写 Rust crate 文档"、"如何审 axum handler" |
| **MCP**    | 高 + 网络     | 模型显式调用                | Stripe Toolshed 风格的跨工程内部工具集            |

**关键区分**：

- **Hook / Guide / Sensor** = 框架在确定性事件 / 任务 scope / 行动后自动触发
- **Skill / MCP** = 模型自己决定何时调用

→ 优先选 **左边**；只在更轻量级满足不了时升级。Skill 走 [agentskills.io](https://agentskills.io/specification) 规范以保证跨 agent 可移植。

---

## 13. 与 Rig / AutoAgents 互操作

`harness-models` 提供薄适配层：

```rust
// 把 rig::completion::CompletionModel 适配成 harness::Model
pub struct RigAdapter<M: rig::completion::CompletionModel>(pub M);

#[async_trait]
impl<M: rig::completion::CompletionModel + Send + Sync + 'static> Model for RigAdapter<M> {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> { ... }
    // ...
}

// 把 rig::tool::Tool 适配成 harness::Tool
pub struct RigToolAdapter<T: rig::tool::Tool>(pub T);
```

类似的，AutoAgents 的 ReAct executor 可以包装成一个 `Node::Agent` 后端。这意味着用户**完全不必从 0 重写工具**——已经在 Rig 生态里的 tool 直接可用。

---

## 14. CLI

```bash
harness new my-minion                    # 用模板生成新工程
harness run ./blueprint.toml             # 执行一个 blueprint
harness run -i                           # 交互模式 (人在回路)
harness diff                             # 显示当前 harness 配置 vs 上次
harness guide add ./skills/api.md        # 注册新 guide
harness sensor add clippy --stage SelfCorrect
harness sensor promote review --stage PostIntegrate
harness skills list
harness skills show writing-plans
harness compactor stats                  # 上次 session 各阶段触发次数
harness trace ./session.jsonl            # 回放
```

配置存进仓库的 `harness.toml` + `harness/` 目录，作为可 review、可 diff 的工程产物。

---

## 15. 路线图

### MVP (v0.0.1)

- [ ] `harness-core` — 全部 trait + 类型（含 `Skill` trait + `SkillManifest`）
- [ ] `harness-macros` — `#[skill]` `#[tool]` 基础版
- [ ] `harness-skills` — `skills_dir!` 宏 + spec-validator (agentskills.io 兼容)
- [ ] `harness-loop` — 单 Agent 节点的最小可跑 loop（含内置 `activate_skill` tool）
- [ ] `harness-context` — 简单上下文 (无压缩)
- [ ] `harness-models` — Anthropic 适配 (走 Rig)
- [ ] `harness-tools-fs` + `harness-tools-shell`
- [ ] `harness-sensors-rust` — `cargo check` + `clippy`
- [ ] `harness-cli` — `harness run`、`harness skills validate`
- [ ] `examples/crate-keeper` — 端到端跑通: bump deps + 跑 test + 提交

### v0.1 ✅

- [x] Compactor 全部 5 阶段 — `DefaultCompactor` 在 `AgentLoop` 内按预算阈值自动触发
- [x] HookBus + 全部 27 事件 + `#[hook]` 宏 — `HookBus` 在 PreTool/PostTool/Pre…/Post…/SessionStart/SessionEnd/Heartbeat/PreCompact/PostCompact 等点全部触发；PreToolUse `Deny` 短路并把原因喂回模型
- [x] Blueprint 状态机 — `Deterministic` + `Agent` 节点，命名边 + Transition::Next/Done/Edge/Abort，`branch_on_failure` + `retry_cap`
- [x] WorktreeSandbox — 真实 `git worktree add` + drop-time `git worktree remove`；`NullSandbox` 作 fallback；`ContainerSandbox`/`VmSandbox` 留 v0.2
- [x] `#[guide]` `#[sensor]` `#[hook]` `#[tool]` 全部宏 — 与 `#[skill]` 同一 inventory 注册模式
- [x] Subagent — `SubagentSpec` + `Subagent::run` 跑隔离 `AgentLoop`，按 `SubagentStatus` (Done/DoneWithConcerns/Blocked/NeedsContext) 报告
- [x] `harness skills export` — 把所有注册 skill (宏 + 文件系统) 物化为 `<target>/<name>/SKILL.md`，验证后可被 Claude Code/Cursor/Codex 直接消费
- [x] `harness-templates::axum_crud` — `tools()` + `sensors()` + `guides()` + `blueprint(agent_step)` 端到端模板，含 axum/sqlx/tracing 三组约定 guide
- [x] Anthropic 原生 provider — `AnthropicNative` 加 Messages API + content-block tool-calling；`providers::anthropic_{sonnet_46, opus_47, haiku_45}`
- [x] Auto-fix patches 实际落地 — `FixPatch::ReplaceFile/UnifiedDiff/RunCommand` 在 `AgentLoop` 内通过 `World.runner` / fs 真实应用并报告

### v0.2 ✅

- [x] **Session replay** — `SessionRecorder` Hook serialises every lifecycle event to JSONL; `read_session` + `replay_as_mock` reconstruct a deterministic `MockModel` from the log. `harness trace <file>` pretty-prints with stats. `crate-keeper --record <path>` produces live recordings.
- [x] **ContainerSandbox** — `docker run -d --rm` + bind-mount + `docker exec` routing for the world runner. `--network none` by default.
- [x] **VmSandbox** — Firecracker-shaped API with image-path validation; Firecracker process backend stubbed with a clear error so callers can detect the missing infra path without crashing.
- [x] **MCP server** — `harness-mcp` crate ships a JSON-RPC 2.0 server over stdio implementing `initialize`, `tools/list`, `tools/call`, `ping`. Bin: `harness mcp serve --workspace <path>`. Tools exposed: read/write/edit/list_dir/shell_read. Tested round-trips with 6 unit tests.
- [x] **OpenTelemetry** — `harness-hooks` `otel` feature emits OTel spans for session/model/tool/sensor/compaction. Token usage, stop reason, signal counts as attributes. Uses `BoxedSpan` to stay dyn-compatible.
- [x] **Harness linter** — `harness skills lint <dir>` flags short / vague / overlapping descriptions, near-duplicate names, Jaccard keyword overlap > 50%. 5 unit tests.
- [x] **ModelBackedCompactor** — Microcompact + AutoCompact stages call an LLM (typically `deepseek-flash` or `haiku`) for real semantic summarisation; structural strategies still cover BudgetReduce / Snip / ContextCollapse.
- [x] **Streaming tool calls** — `OpenAiCompat::stream()` parses SSE chunks into `ModelDelta::{Text, ToolCallStart, ToolCallArgs, Usage, Stop}` so callers can render incrementally. 2 unit tests over canned SSE bytes.

---

## 16. 开放问题

1. **Skill 描述膨胀**——按 spec, startup 时所有 skill 的 (name, description) 都进 system prompt。一个仓库装 100 个 skill 就是 100×~1KB ≈ 100KB 永久占用。是否需要二级索引或懒加载？短期方案：`harness.toml` 里 `skills.namespace` 白名单。
2. **Auto-fix 优先级**——多 sensor 同时给出 `FixPatch` 且触碰同一文件怎么办？倾向：按注册顺序串行 apply，每次 apply 后让后续 sensor 重新 observe。
3. **Compactor 阶段 4-5 用什么模型**——Microcompact 用主模型还是次级模型？成本与忠实度的关键 trade-off。
4. **Skill spec 不覆盖动态状态**——agentskills.io 假设 skill 是静态资源。我们的 `#[skill]` 结构体形式（有 `Arc<dyn Model>` 等运行时依赖）如何 export 成 spec 合规目录？倾向：export 时只导出 manifest + body + 一段"此 skill 需要在 harness 运行时使用"的说明。
5. **`activate_skill` tool 暴露给模型的颗粒度**——单次激活一个 skill，还是允许批量？批量激活会导致 body 总量超控。倾向：先单次，观察实际使用模式再调整。
6. **如何评估一个 harness 的"覆盖率"**——Böckeler 自己留作开放问题。可能方向：故意注入已知反模式，看 sensor 命中率。

---

## 17. 命名与许可

- crate 命名前缀: `harness-*`
- 顶层 facade crate (re-export): `harness`
- 许可: 暂定 Apache-2.0 OR MIT (Rust 社区默认)
- MSRV: stable 最新 - 2（约 18 个月窗口）

---

> 这份 DESIGN.md 本身是 harness 的一部分：它就是 *Guide* 的极端形式，
> 让未来的人类工程师和 agent 都能在不读全部源码的情况下做出正确决策。
