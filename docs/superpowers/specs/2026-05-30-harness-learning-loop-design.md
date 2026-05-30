# harness-learning-loop — Self-Evolving Skills + Memory (Framework Capability)

**Status:** Approved (brainstorming) → ready for plan
**Date:** 2026-05-30
**Layer:** harness-rs framework (capability A of the Hermes-port roadmap; B=recall done)

## Goal

Give any harness-rs app a "closed learning loop": after a session does real work,
a **background review subagent** looks at the transcript and writes/patches
**skills** (procedural memory) and **memory** (incl. a user-model), so the next
session starts smarter. One builder call:

```rust
AgentLoop::new(model).with_learning_loop(
    LearningConfig::default_toolset(skills_dir, memory.clone(), review_model)
)
```

Faithful to Hermes Agent's design: a nudge counter triggers a forked review
agent, white-listed to skill-write + memory-write tools, running a strong review
prompt against the conversation.

## Decisions (from brainstorming)

| Question | Decision |
|---|---|
| Where does the review run? | **AgentLoop first-class** `.with_learning_loop(cfg)`; forks at `SessionEnd` |
| Review subagent's model | from `LearningConfig.review_model: Arc<dyn Model>` (app picks; often cheaper) |
| Model-fork compile path | add blanket **`impl<T: Model + ?Sized> Model for Arc<T>`** to harness-core |
| Who builds the white-listed tools? | **the app injects them** via `LearningConfig.tools` (keeps harness-loop decoupled) |
| Trigger | **per-run**: at `SessionEnd`, if `tools_called >= cfg.nudge_interval` |
| Review passes | **one combined** review subagent run (skills + memory together), not two |
| Execution | **inline at SessionEnd** (async, in the loop) — simplest + testable |
| User-model (#4) | reuse existing `Memory` + `RememberThisTool` (collapses into the same fork) |
| Best-effort | a review failure NEVER affects the just-finished run |

## Prerequisite: blanket `Model` impl (harness-core)

`Model` is object-safe but `Arc<dyn Model>` doesn't implement `Model`, so it can't
be passed to `Subagent::new(model: M)`. Add (in `crates/harness-core/src/model.rs`):

```rust
#[async_trait::async_trait]
impl<T: Model + ?Sized> Model for std::sync::Arc<T> {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        (**self).complete(ctx).await
    }
    async fn stream(
        &self,
        ctx: &Context,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        (**self).stream(ctx).await
    }
    fn info(&self) -> ModelInfo {
        (**self).info()
    }
}
```

This is a general win: anyone can now build `AgentLoop::new(arc_model)` or pass a
boxed model to a `Subagent`.

## Crate plan

| Component | Crate | New deps |
|---|---|---|
| `impl Model for Arc<T>` | **harness-core** | none |
| `LearningConfig` + `.with_learning_loop()` + SessionEnd fork + default prompt consts | **harness-loop** | none (tools injected; review_model is `Arc<dyn Model>`) |
| skill **write** path: `write_skill_md`, `delete_skill` (validate-on-write) | **harness-skills** (today read-only) | none |
| `SkillManageTool` (LLM-facing create/patch/edit/delete) | **NEW `crates/harness-tools-skills`** | harness-core, harness-skills |
| user-model | reuse `harness-tools-memory::RememberThisTool` + `Memory` | — |

harness-loop stays decoupled: it never depends on harness-skills/tools-memory —
the app constructs the white-listed tools and hands them in via `LearningConfig`.

## `LearningConfig` (harness-loop)

```rust
pub struct LearningConfig {
    /// Model the review subagent runs on (often a cheaper one). Object-safe via
    /// the blanket Arc impl, so `Arc::new(my_model)` works.
    pub review_model: std::sync::Arc<dyn harness_core::Model>,
    /// The ONLY tools the review subagent may call (the white-list): typically a
    /// SkillManageTool + a RememberThisTool. The app builds + injects these.
    pub tools: Vec<std::sync::Arc<dyn harness_core::Tool>>,
    /// Review prompt (defaults to DEFAULT_REVIEW_PROMPT — Hermes-derived).
    pub review_prompt: String,
    /// Fire the review only if the finished run made >= this many tool calls.
    pub nudge_interval: u32,
    /// Iteration cap for the review subagent.
    pub max_iters: u32,
}

impl LearningConfig {
    /// Minimal: review_model + an empty tool white-list (add tools yourself).
    pub fn new(review_model: Arc<dyn Model>) -> Self { /* nudge=10, max_iters=6, DEFAULT_REVIEW_PROMPT */ }
    pub fn with_tool(mut self, t: Arc<dyn Tool>) -> Self { self.tools.push(t); self }
    pub fn with_nudge_interval(mut self, n: u32) -> Self { self.nudge_interval = n; self }
    pub fn with_review_prompt(mut self, p: impl Into<String>) -> Self { self.review_prompt = p.into(); self }
    pub fn with_max_iters(mut self, n: u32) -> Self { self.max_iters = n; self }
}
```

`default_toolset` is a convenience that lives in the **`harness` facade crate**
(which already depends on tools-skills + tools-memory), so harness-loop stays
clean:
```rust
// in crates/harness/src/...
pub fn learning_default(skills_dir: PathBuf, memory: Arc<dyn Memory>, review_model: Arc<dyn Model>) -> LearningConfig {
    LearningConfig::new(review_model)
        .with_tool(Arc::new(harness_tools_skills::SkillManageTool::new(skills_dir)))
        .with_tool(Arc::new(harness_tools_memory::RememberThisTool::new(memory)))
}
```

## AgentLoop wiring (harness-loop)

- Field: `pub learning: Option<LearningConfig>`.
- Builder: `pub fn with_learning_loop(mut self, cfg: LearningConfig) -> Self { self.learning = Some(cfg); self }`.
- In `run_built_context`, at BOTH `SessionEnd` points (natural Done + BudgetExhausted),
  BEFORE returning, call a best-effort helper:

```rust
    async fn run_learning_review(&self, ctx: &Context, world: &mut World, tools_called: u32) {
        let Some(cfg) = &self.learning else { return };
        if tools_called < cfg.nudge_interval { return; }
        let transcript = render_transcript(&ctx.history); // role-tagged text
        let task = Task {
            description: format!("{}\n\n## Conversation transcript\n{}", cfg.review_prompt, transcript),
            source: None, deadline: None,
        };
        let mut spec = SubagentSpec::new("learning-review", task).with_max_iters(cfg.max_iters);
        for t in &cfg.tools { spec = spec.with_tool(t.clone()); }
        let sub = Subagent::new(cfg.review_model.clone(), spec);
        if let Err(e) = sub.run(world).await {
            tracing::warn!(error = %e, "learning review failed");
        }
    }
```

Call sites: right after `self.hooks.fire(&Event::SessionEnd, world);` in each path,
`self.run_learning_review(&ctx, world, tools_called).await;` (Done path uses
`iter+1`-era `tools_called`; Budget path uses its `tools_called`). The review's own
tool calls do NOT recurse into another review (the subagent is a plain AgentLoop
with no `learning` set).

`render_transcript(history: &[Turn]) -> String`: walk turns, emit
`role: text` / `tool(call_id): <json>` lines. Cap to a sane length (e.g. last N
turns / char budget) so a huge session doesn't blow the review context.

## Default review prompt (harness-loop const)

`DEFAULT_REVIEW_PROMPT` — the verbatim Hermes `_SKILL_REVIEW_PROMPT` (class-level
umbrella skills; references/templates/scripts; "most sessions produce ≥1 update;
'Nothing to save.' is allowed but not the default") MERGED with the memory clause
from `_MEMORY_REVIEW_PROMPT` (save user persona/preferences/working-style via the
memory tool). Plus a 1-line framing: "You are a background reviewer. Using ONLY
the tools provided (skill + memory management), update the skill library and
memory based on the conversation transcript below. Make at most a few focused
changes. If nothing is worth saving, do nothing." Overridable via
`LearningConfig.with_review_prompt`.

## skill write path (harness-skills)

Add to harness-skills (it already has `export_one` + `validate` + the loader):

```rust
/// Write `content` (a full SKILL.md: frontmatter + body) to <dir>/<name>/SKILL.md,
/// creating the dir. Validates by loading it back; on validation failure the file
/// is removed and the error returned (no half-written invalid skill survives).
pub fn write_skill_md(dir: &Path, name: &str, content: &str) -> Result<PathBuf, SkillError>;

/// Remove <dir>/<name>/ entirely. Ok if absent.
pub fn delete_skill(dir: &Path, name: &str) -> Result<bool, SkillError>;
```

`name` is sanitized (lowercase, `[a-z0-9-]`, no traversal) consistent with the
agentskills.io rule already enforced by `validate_name`.

## `SkillManageTool` (new crate `harness-tools-skills`)

Mirrors `RememberThisTool`: holds a `skills_dir: PathBuf`, constructed at wiring
time. `name = "skill_manage"`, `risk = Destructive`. Schema (Hermes-aligned):

```json
{ "action": "create|patch|edit|delete",
  "name": "lowercase-hyphenated",
  "content": "full SKILL.md (for create/edit)",
  "old_string": "for patch", "new_string": "for patch" }
```

- `create` / `edit`: `write_skill_md(dir, name, content)`.
- `patch`: read `<dir>/<name>/SKILL.md`, replace first `old_string`→`new_string`
  (error if not found / not unique), `write_skill_md` the result.
- `delete`: `delete_skill(dir, name)`.

Returns `ToolResult { ok, content: {action, name, path?} }`. New crate
`crates/harness-tools-skills` (`[package] name = harness-rs-tools-skills`,
`[lib] name = harness_tools_skills`), deps harness-core + harness-skills.

## Error handling

| Situation | Behavior |
|---|---|
| `learning` not set | zero overhead; loop unchanged |
| `tools_called < nudge_interval` | no review |
| review subagent errors / model fails | `tracing::warn!`, run still returns its real `Outcome` |
| skill_manage validation fails | tool returns `ok:false` (the review agent sees it, can retry) |
| review recursion | impossible — the subagent's AgentLoop has no `learning` |

## Testing

- **Blanket Model impl**: `Arc<dyn Model>` is usable where `M: Model` is required
  (build a `Subagent`/`AgentLoop` from one).
- **skill write path**: `write_skill_md` round-trips + `load_skill_dir` reads it;
  invalid frontmatter → error + no file left; `delete_skill`.
- **SkillManageTool**: create writes a valid SKILL.md; patch replaces; delete
  removes; bad name rejected.
- **Trigger threshold**: with `nudge_interval=2`, a run with 1 tool call does NOT
  review; a run with ≥2 does.
- **End-to-end fork** (the headline test): a `MockMainModel` does 2 tool calls then
  finishes; a `MockReviewModel` (the `review_model`) emits a `skill_manage` create
  call then stops; assert a SKILL.md was written to the temp skills dir AND the run
  returned its normal `Outcome::Done`. A second variant: `MockReviewModel` errors →
  the run STILL returns `Done` (best-effort).
- **No-recursion**: the review subagent itself has no learning loop (assert it
  doesn't spawn a nested review — e.g. the review model is called a bounded number
  of times).

## Out of scope (v1)

- Detached/fire-and-forget background thread (inline-at-SessionEnd only; revisit
  if review latency matters).
- Mid-session periodic nudges (Hermes nudges every N iters *within* a session;
  v1 reviews once at session end).
- Two separate skill/memory passes (one combined run).
- Skill self-improvement as a distinct mechanism — the agent patching a skill it
  used is just normal `skill_manage` usage, no special path.
- Honcho-style dialectic user-model (the local Memory-based user-model suffices).

## Dogfood / reference consumer

Wire `examples/dashboard`: a `skills_dir` per deploy + the existing per-user
`Memory`, a cheap `review_model` (deepseek-v4-flash), `nudge_interval` tuned, and
confirm that after a multi-tool chat the review writes a skill/memory entry.
Doubles as the usage example.
