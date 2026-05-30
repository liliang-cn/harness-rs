# harness-learning-loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in self-evolving "learning loop" to harness-rs: after a session does real work, a forked review subagent (white-listed to skill-write + memory-write tools) reviews the transcript and writes/patches skills + memory — `AgentLoop::new(model).with_learning_loop(cfg)`.

**Architecture:** A blanket `impl Model for Arc<T>` makes a boxed review model usable by a `Subagent`. `LearningConfig` (review_model + injected white-list tools + review prompt + nudge threshold) lives in harness-loop; at `SessionEnd`, if `tools_called >= nudge_interval`, the loop forks a review subagent inline (best-effort). The skill-write path is added to harness-skills; a new `harness-tools-skills` crate provides the LLM-facing `SkillManageTool`. Memory/user-model reuses the existing `Memory` + `RememberThisTool`.

**Tech Stack:** Rust, async-trait, serde_json; reuses harness-core/loop/skills/tools-memory. No new external deps.

**Spec:** `docs/superpowers/specs/2026-05-30-harness-learning-loop-design.md`

**Conventions (verified):**
- Crate `[package] name = harness-rs-<x>`, dir `crates/harness-<x>`, deps via workspace alias.
- `Model` trait (harness-core/src/model.rs): `async fn complete(&self,&Context)->Result<ModelOutput,ModelError>; async fn stream(...) -> Result<BoxStream<...>,ModelError> (default); fn info(&self)->ModelInfo;` — object-safe.
- `SubagentSpec::new(name, task).with_tool(Arc<dyn Tool>).with_max_iters(n)`; `Subagent::new(model: M, spec).run(&mut World) -> Result<SubagentReport, HarnessError>` (harness-loop/src/subagent.rs).
- `Task { description: String, source: Option<String>, deadline: Option<i64> }`. `Turn { role: TurnRole, blocks: Vec<Block> }`; `TurnRole::{User,Assistant,System,Tool}`; `Block::Text(String)`, `Block::ToolResult { call_id, content }`, `Block::ToolCall {..}`.
- harness-skills: `loader::load(&Path)->Result<FileSkill,SkillError>`, `load_skill_dir(&Path)->Arc<dyn Skill>`, `validate::validate(&SkillManifest)`, `validate::validate_name(&str)`, `export::render_skill_md(&SkillManifest,&str)`. `SkillError::{Io(String), Invalid{..}, MissingField{field}, NameRegex{..}, ...}`.
- `RememberThisTool::new(Arc<dyn Memory>)` (harness-tools-memory).
- Tool: `fn name/schema/risk; async fn invoke(&self, Value, &mut World)->Result<ToolResult,ToolError>`. `ToolResult{ok,content,trace}`. `ToolError::{InvalidArgs{name,reason}, Exec(String)}`. `ToolRisk::Destructive`.
- Run tests: `cargo test -p <crate> <filter>`. NO Co-Authored-By / AI attribution in commits.

---

### Task 1: blanket `impl Model for Arc<T>` (harness-core)

**Files:** Modify `crates/harness-core/src/model.rs`.

- [ ] **Step 1: Add the impl**

After the `Model` trait definition in `crates/harness-core/src/model.rs`, add:

```rust
/// Lets a boxed/shared model (`Arc<dyn Model>`) be used anywhere a `Model` is
/// required — e.g. as the concrete `M` in `AgentLoop<M>` / `Subagent<M>`. The
/// `Model` trait is object-safe, so this just forwards to the inner value.
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

Ensure `Context`, `ModelOutput`, `ModelError`, `ModelDelta`, `ModelInfo` are in scope in that module (they are — the trait uses them).

- [ ] **Step 2: Add a test**

Append to `crates/harness-core/src/model.rs` (in or after an existing `#[cfg(test)] mod tests`, create one if absent):

```rust
#[cfg(test)]
mod arc_model_tests {
    use super::*;
    use std::sync::Arc;

    struct Dummy;
    #[async_trait::async_trait]
    impl Model for Dummy {
        async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
            Ok(ModelOutput { text: Some("ok".into()), tool_calls: vec![], usage: Usage::default(), stop_reason: StopReason::EndTurn, reasoning: None })
        }
        fn info(&self) -> ModelInfo {
            ModelInfo { handle: "dummy".into(), provider: "test".into(), model: "dummy".into(), context_window: 8192, input_cost_usd_per_million_tokens: None, output_cost_usd_per_million_tokens: None, supports_tool_use: false, supports_streaming: false }
        }
    }

    fn assert_is_model<M: Model>(_m: &M) {}

    #[tokio::test]
    async fn arc_dyn_model_is_a_model() {
        let m: Arc<dyn Model> = Arc::new(Dummy);
        assert_is_model(&m); // compiles only if Arc<dyn Model>: Model
        let out = m.complete(&Context::new(crate::Task { description: "x".into(), source: None, deadline: None })).await.unwrap();
        assert_eq!(out.text.as_deref(), Some("ok"));
    }
}
```

Adapt `ModelOutput`/`Usage`/`StopReason`/`ModelInfo` field construction to the real shapes if these literals differ (check the surrounding model.rs). `ModelInfo::default()` may need real fields — use whatever constructs a minimal valid `ModelInfo`. The KEY assertion is `assert_is_model(&m)` compiling.

If harness-core has no `tokio` dev-dep, add `tokio = { workspace = true, features = ["macros","rt-multi-thread"] }` under `[dev-dependencies]` in `crates/harness-core/Cargo.toml` (do NOT add to normal deps).

- [ ] **Step 3: Build + test**

Run: `cargo test -p harness-rs-core arc_dyn_model` and `cargo build -p harness-rs-core`
Expected: PASS; harness-core's normal deps unchanged.

- [ ] **Step 4: Commit**

```bash
git add crates/harness-core/src/model.rs crates/harness-core/Cargo.toml
git commit -m "feat(harness-core): blanket impl Model for Arc<T> (boxed models usable as M)"
```

---

### Task 2: skill write path (harness-skills)

**Files:** Create `crates/harness-skills/src/write.rs`; modify `crates/harness-skills/src/lib.rs` (`pub mod write; pub use write::*;`).

- [ ] **Step 1: Write the module**

Create `crates/harness-skills/src/write.rs`:

```rust
//! Programmatic skill writing — create/overwrite/delete a `<dir>/<name>/SKILL.md`
//! with validate-on-write. Used by the `skill_manage` tool (learning loop) so an
//! agent can author skills at runtime. Read paths live in `loader`/`registry`.

use harness_core::SkillError;
use std::path::{Path, PathBuf};

/// Write a full SKILL.md (`content` = frontmatter + body) to `<dir>/<name>/SKILL.md`.
///
/// Validates the result by loading it back; if the written skill is invalid, the
/// skill directory is removed and the error returned — no half-written invalid
/// skill survives. `name` must pass agentskills.io name rules.
pub fn write_skill_md(dir: &Path, name: &str, content: &str) -> Result<PathBuf, SkillError> {
    crate::validate::validate_name(name)?;
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).map_err(|e| SkillError::Io(e.to_string()))?;
    let path = skill_dir.join("SKILL.md");
    // Snapshot any prior content so we can roll back a bad overwrite.
    let prior = std::fs::read(&path).ok();
    std::fs::write(&path, content).map_err(|e| SkillError::Io(e.to_string()))?;
    match crate::loader::load(&skill_dir) {
        Ok(_) => Ok(path),
        Err(e) => {
            // roll back
            match prior {
                Some(bytes) => { let _ = std::fs::write(&path, bytes); }
                None => { let _ = std::fs::remove_dir_all(&skill_dir); }
            }
            Err(e)
        }
    }
}

/// Remove `<dir>/<name>/` entirely. Returns `true` if it existed.
pub fn delete_skill(dir: &Path, name: &str) -> Result<bool, SkillError> {
    crate::validate::validate_name(name)?;
    let skill_dir = dir.join(name);
    if skill_dir.exists() {
        std::fs::remove_dir_all(&skill_dir).map_err(|e| SkillError::Io(e.to_string()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-skillwrite-{}-{n}", std::process::id()))
    }

    const VALID: &str = "---\nname: deploy-runbook\ndescription: How to deploy the service.\n---\n# Deploy\n1. build\n2. ship\n";

    #[test]
    fn write_then_load_roundtrips() {
        let dir = tmp();
        let p = write_skill_md(&dir, "deploy-runbook", VALID).unwrap();
        assert!(p.exists());
        let loaded = crate::loader::load(&dir.join("deploy-runbook")).unwrap();
        assert_eq!(loaded.manifest().name, "deploy-runbook");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_content_rolls_back_and_errors() {
        let dir = tmp();
        // missing frontmatter / required fields → load() should reject
        let bad = "no frontmatter here";
        let err = write_skill_md(&dir, "broken", bad);
        assert!(err.is_err(), "invalid skill must error");
        assert!(!dir.join("broken").join("SKILL.md").exists(), "no file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_name_rejected() {
        let dir = tmp();
        assert!(write_skill_md(&dir, "Bad Name!", VALID).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_dir() {
        let dir = tmp();
        write_skill_md(&dir, "deploy-runbook", VALID).unwrap();
        assert!(delete_skill(&dir, "deploy-runbook").unwrap());
        assert!(!delete_skill(&dir, "deploy-runbook").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

> Verify `crate::loader::load` rejects content without valid frontmatter (read
> `loader.rs::parse_frontmatter`). If `load` is lenient (accepts no-frontmatter),
> make `write_skill_md` additionally call `validate::validate(loaded.manifest())`
> after load and treat a validation error as the rollback trigger, so
> `invalid_content_rolls_back_and_errors` holds. Adjust to guarantee an invalid
> skill cannot be written.

- [ ] **Step 2: Wire lib.rs**

In `crates/harness-skills/src/lib.rs`: add `pub mod write;` (after `pub mod validate;`) and `pub use write::*;` (after `pub use validate::*;`).

- [ ] **Step 3: Build + test**

Run: `cargo test -p harness-rs-skills write` and `cargo build -p harness-rs-skills`
Expected: 4 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/harness-skills/src/write.rs crates/harness-skills/src/lib.rs
git commit -m "feat(harness-skills): write_skill_md + delete_skill (validate-on-write)"
```

---

### Task 3: `harness-tools-skills` crate + `SkillManageTool`

**Files:** Create `crates/harness-tools-skills/Cargo.toml` + `src/lib.rs`; add member to root `Cargo.toml`.

- [ ] **Step 1: Manifest**

Create `crates/harness-tools-skills/Cargo.toml`:

```toml
[package]
name = "harness-rs-tools-skills"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
description = "LLM-facing skill-authoring tool for harness-rs agents: skill_manage (create/patch/edit/delete SKILL.md). Used by the learning loop."

[lib]
name = "harness_tools_skills"

[dependencies]
harness-core   = { workspace = true }
harness-skills = { workspace = true }
async-trait    = { workspace = true }
serde_json     = { workspace = true }

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
harness-context = { workspace = true }
```

Add `"crates/harness-tools-skills",` to `members` in root `Cargo.toml` (next to `"crates/harness-tools-memory",`).

- [ ] **Step 2: Implement `SkillManageTool`**

Create `crates/harness-tools-skills/src/lib.rs`:

```rust
//! `skill_manage` — the LLM-facing tool that lets an agent author its own skills
//! (create/patch/edit/delete SKILL.md). State-bearing (holds the skills dir), so
//! constructed at wiring time like `RememberThisTool`. Risk = Destructive.

use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct SkillManageTool {
    dir: PathBuf,
    schema: ToolSchema,
}

impl SkillManageTool {
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: skills_dir.into(),
            schema: ToolSchema {
                name: "skill_manage".into(),
                description: "Author your procedural memory as reusable skills. \
                    actions: create (write a new SKILL.md), edit (overwrite an existing one), \
                    patch (replace old_string→new_string in a skill), delete. \
                    A skill is a SKILL.md with YAML frontmatter (name, description) + a \
                    markdown body of numbered steps + pitfalls. Use class-level names \
                    (e.g. 'deploy-runbook', not 'fix-bug-1234')."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "edit", "patch", "delete"]},
                        "name": {"type": "string", "description": "lowercase-hyphenated skill name"},
                        "content": {"type": "string", "description": "full SKILL.md (frontmatter + body) for create/edit"},
                        "old_string": {"type": "string", "description": "exact text to replace, for patch"},
                        "new_string": {"type": "string", "description": "replacement text, for patch"}
                    },
                    "required": ["action", "name"]
                }),
            },
        }
    }

    fn arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
        args.get(k).and_then(|v| v.as_str())
    }
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str { &self.schema.name }
    fn schema(&self) -> &ToolSchema { &self.schema }
    fn risk(&self) -> ToolRisk { ToolRisk::Destructive }

    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let action = Self::arg(&args, "action").ok_or_else(|| ToolError::InvalidArgs { name: "skill_manage".into(), reason: "action required".into() })?;
        let name = Self::arg(&args, "name").ok_or_else(|| ToolError::InvalidArgs { name: "skill_manage".into(), reason: "name required".into() })?;

        let result: Result<Value, String> = match action {
            "create" | "edit" => {
                let content = Self::arg(&args, "content").ok_or_else(|| ToolError::InvalidArgs { name: "skill_manage".into(), reason: "content required for create/edit".into() })?;
                harness_skills::write_skill_md(&self.dir, name, content)
                    .map(|p| json!({"action": action, "name": name, "path": p.to_string_lossy()}))
                    .map_err(|e| e.to_string())
            }
            "patch" => {
                let old = Self::arg(&args, "old_string").ok_or_else(|| ToolError::InvalidArgs { name: "skill_manage".into(), reason: "old_string required for patch".into() })?;
                let new = Self::arg(&args, "new_string").unwrap_or("");
                let path = self.dir.join(name).join("SKILL.md");
                match std::fs::read_to_string(&path) {
                    Ok(cur) => {
                        let matches = cur.matches(old).count();
                        if matches == 0 {
                            Err(format!("old_string not found in {name}"))
                        } else if matches > 1 {
                            Err(format!("old_string not unique in {name} ({matches} matches)"))
                        } else {
                            let patched = cur.replacen(old, new, 1);
                            harness_skills::write_skill_md(&self.dir, name, &patched)
                                .map(|p| json!({"action": "patch", "name": name, "path": p.to_string_lossy()}))
                                .map_err(|e| e.to_string())
                        }
                    }
                    Err(e) => Err(format!("read {name}: {e}")),
                }
            }
            "delete" => {
                harness_skills::delete_skill(&self.dir, name)
                    .map(|removed| json!({"action": "delete", "name": name, "removed": removed}))
                    .map_err(|e| e.to_string())
            }
            other => Err(format!("unknown action `{other}`")),
        };

        match result {
            Ok(content) => Ok(ToolResult { ok: true, content, trace: None }),
            Err(reason) => Ok(ToolResult { ok: false, content: json!({"error": reason}), trace: None }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::default_world;

    fn tmp() -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-skillmanage-{}-{n}", std::process::id()))
    }

    const SKILL: &str = "---\nname: deploy-runbook\ndescription: How to deploy.\n---\n# Deploy\n1. build\n";

    #[tokio::test]
    async fn create_patch_delete() {
        let dir = tmp();
        let tool = SkillManageTool::new(&dir);
        let mut w = default_world(".");

        let out = tool.invoke(json!({"action":"create","name":"deploy-runbook","content": SKILL}), &mut w).await.unwrap();
        assert!(out.ok, "create: {:?}", out.content);
        assert!(dir.join("deploy-runbook").join("SKILL.md").exists());

        let out = tool.invoke(json!({"action":"patch","name":"deploy-runbook","old_string":"1. build","new_string":"1. build\n2. test"}), &mut w).await.unwrap();
        assert!(out.ok, "patch: {:?}", out.content);
        let body = std::fs::read_to_string(dir.join("deploy-runbook").join("SKILL.md")).unwrap();
        assert!(body.contains("2. test"));

        let out = tool.invoke(json!({"action":"delete","name":"deploy-runbook"}), &mut w).await.unwrap();
        assert!(out.ok);
        assert!(!dir.join("deploy-runbook").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bad_name_returns_ok_false() {
        let dir = tmp();
        let tool = SkillManageTool::new(&dir);
        let mut w = default_world(".");
        let out = tool.invoke(json!({"action":"create","name":"Bad Name","content": SKILL}), &mut w).await.unwrap();
        assert!(!out.ok);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 3: Build + test**

Run: `cargo test -p harness-rs-tools-skills` and `cargo build -p harness-rs-tools-skills`
Expected: 2 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/harness-tools-skills Cargo.toml
git commit -m "feat(harness-tools-skills): skill_manage tool (create/edit/patch/delete)"
```

---

### Task 4: `LearningConfig` + transcript + default prompt + review helper (harness-loop)

**Files:** Create `crates/harness-loop/src/learning.rs`; modify `crates/harness-loop/src/lib.rs` (`pub mod learning; pub use learning::*;` + AgentLoop field + builder + the helper method).

- [ ] **Step 1: Create the learning module (config + transcript + prompt)**

Create `crates/harness-loop/src/learning.rs`:

```rust
//! Self-evolving learning loop for [`crate::AgentLoop`].
//!
//! After a session does real work, a forked review subagent — white-listed to
//! skill-write + memory-write tools — reviews the transcript and writes/patches
//! skills + memory. See [`LearningConfig`] and `AgentLoop::with_learning_loop`.

use harness_core::{Block, Model, Tool, Turn, TurnRole};
use std::sync::Arc;

/// Configuration for the learning loop. The app injects the review model + the
/// white-listed tools the review subagent may call (typically a `SkillManageTool`
/// + a `RememberThisTool`); harness-loop never depends on those crates.
pub struct LearningConfig {
    pub review_model: Arc<dyn Model>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub review_prompt: String,
    pub nudge_interval: u32,
    pub max_iters: u32,
}

impl LearningConfig {
    pub fn new(review_model: Arc<dyn Model>) -> Self {
        Self {
            review_model,
            tools: Vec::new(),
            review_prompt: DEFAULT_REVIEW_PROMPT.to_string(),
            nudge_interval: 10,
            max_iters: 6,
        }
    }
    pub fn with_tool(mut self, t: Arc<dyn Tool>) -> Self { self.tools.push(t); self }
    pub fn with_nudge_interval(mut self, n: u32) -> Self { self.nudge_interval = n; self }
    pub fn with_review_prompt(mut self, p: impl Into<String>) -> Self { self.review_prompt = p.into(); self }
    pub fn with_max_iters(mut self, n: u32) -> Self { self.max_iters = n; self }
}

/// Default review prompt — adapted from Hermes Agent's skill+memory review.
pub const DEFAULT_REVIEW_PROMPT: &str = "\
You are a BACKGROUND REVIEWER running after a session finished. Using ONLY the \
tools provided (skill management + memory), update the skill library and memory \
based on the conversation transcript below. Make at most a few focused changes.\n\n\
Be active — most sessions that did real work produce at least one small update; a \
pass that does nothing is a missed learning opportunity, not a neutral outcome. \
But 'nothing to save' IS a valid result for a trivial session — if so, do nothing.\n\n\
SKILLS (procedural memory): when a non-trivial technique, fix, workflow, or \
correction emerged that a future session would reuse, capture it as a skill with \
skill_manage. Prefer CLASS-LEVEL umbrella skills with a rich body (trigger \
conditions, numbered steps with exact commands, a pitfalls section). The name must \
be class-level (e.g. 'deploy-runbook'), NEVER a one-off ('fix-bug-1234'). If an \
existing skill covers the territory, PATCH it (add a step or pitfall) instead of \
creating a new one.\n\n\
MEMORY (about the user): if the user revealed durable preferences, working style, \
identity, or expectations about how you should behave ('stop doing X', 'always Y', \
'remember Z'), save them with the memory tool so the next session starts knowing.\n\n\
Make your changes, then stop.";

/// Render conversation history into a compact, role-tagged transcript for the
/// reviewer. Keeps the TAIL within a char budget (recent turns matter most).
pub fn render_transcript(history: &[Turn], max_chars: usize) -> String {
    let mut out = String::new();
    for turn in history {
        let role = match turn.role {
            TurnRole::User => "user",
            TurnRole::Assistant => "assistant",
            TurnRole::System => "system",
            TurnRole::Tool => "tool",
        };
        for b in &turn.blocks {
            match b {
                Block::Text(t) => out.push_str(&format!("{role}: {t}\n")),
                Block::ToolResult { content, .. } => out.push_str(&format!("tool_result: {content}\n")),
                _ => {} // ToolCall etc. — omit from the review transcript
            }
        }
    }
    if out.len() > max_chars {
        // keep the tail, on a char boundary
        let start = out.len() - max_chars;
        let start = (start..out.len()).find(|i| out.is_char_boundary(*i)).unwrap_or(out.len());
        out = format!("…(transcript truncated)…\n{}", &out[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_renders_roles_and_truncates_tail() {
        let history = vec![
            Turn { role: TurnRole::User, blocks: vec![Block::Text("hello there".into())] },
            Turn { role: TurnRole::Assistant, blocks: vec![Block::Text("hi".into())] },
        ];
        let t = render_transcript(&history, 10_000);
        assert!(t.contains("user: hello there"));
        assert!(t.contains("assistant: hi"));

        let big = vec![Turn { role: TurnRole::User, blocks: vec![Block::Text("x".repeat(50_000))] }];
        let t = render_transcript(&big, 1_000);
        assert!(t.len() < 1_200);
        assert!(t.starts_with("…(transcript truncated)…"));
    }
}
```

- [ ] **Step 2: Wire module + AgentLoop field/builder + review helper**

In `crates/harness-loop/src/lib.rs`:
- Add `pub mod learning;` + `pub use learning::*;` near the other module decls.
- Add field to `struct AgentLoop<M>` (after `pub recall_auto_inject: bool,`): `pub learning: Option<LearningConfig>,`.
- In `AgentLoop::new`, init: `learning: None,`.
- Builder (after `auto_inject`):
```rust
    /// Enable the self-evolving learning loop: after a session that made
    /// `>= cfg.nudge_interval` tool calls, fork a review subagent (white-listed to
    /// `cfg.tools`) to update skills + memory from the transcript. Best-effort.
    pub fn with_learning_loop(mut self, cfg: LearningConfig) -> Self {
        self.learning = Some(cfg);
        self
    }
```
- Add the helper inside `impl<M: Model> AgentLoop<M>`:
```rust
    /// Best-effort post-session review. Never affects the finished run.
    async fn run_learning_review(&self, ctx: &Context, world: &mut World, tools_called: u32) {
        let Some(cfg) = &self.learning else { return };
        if tools_called < cfg.nudge_interval { return; }
        let transcript = crate::render_transcript(&ctx.history, 12_000);
        let task = harness_core::Task {
            description: format!("{}\n\n## Conversation transcript\n{}", cfg.review_prompt, transcript),
            source: None,
            deadline: None,
        };
        let mut spec = crate::SubagentSpec::new("learning-review", task).with_max_iters(cfg.max_iters);
        for t in &cfg.tools {
            spec = spec.with_tool(t.clone());
        }
        let sub = crate::Subagent::new(cfg.review_model.clone(), spec);
        if let Err(e) = sub.run(world).await {
            tracing::warn!(error = %e, "learning review failed");
        }
    }
```
(`Context` is already imported in lib.rs; `Subagent`/`SubagentSpec` are in this crate.)

- [ ] **Step 3: Build (call sites come in Task 5)**

Run: `cargo test -p harness-rs-loop learning` and `cargo build -p harness-rs-loop`
Expected: transcript test PASS; clean build (the `run_learning_review` helper is unused until Task 5 — allow `#[allow(dead_code)]` on it ONLY if the unused warning blocks a warning-free build; otherwise leave it and Task 5 wires it immediately).

- [ ] **Step 4: Commit**

```bash
git add crates/harness-loop/src/learning.rs crates/harness-loop/src/lib.rs
git commit -m "feat(harness-loop): LearningConfig + render_transcript + default review prompt + review helper"
```

---

### Task 5: wire review into SessionEnd + end-to-end fork test

**Files:** Modify `crates/harness-loop/src/lib.rs` (call `run_learning_review` at both SessionEnd points); create `crates/harness-loop/tests/learning_loop.rs`.

- [ ] **Step 1: Call the review at both SessionEnd points**

In `run_built_context`:
- Natural Done path: the block is
  ```rust
            if out.tool_calls.is_empty() {
                self.hooks.fire(&Event::TaskCompleted, world);
                self.hooks.fire(&Event::SessionEnd, world);
                return Ok(Outcome::Done { text: out.text, iters: iter + 1, tools_called, usage: total_usage });
            }
  ```
  Insert the review BEFORE the `return`, after the `SessionEnd` hook:
  ```rust
                self.run_learning_review(&ctx, world, tools_called).await;
  ```
  NOTE: `out.text` is moved into `Outcome::Done`. The review uses `&ctx` (which already contains the assistant turn via the earlier `ctx.push_model_output(&out)`), so it does not need `out`. Place the `run_learning_review` call before constructing the `Outcome::Done` (it only borrows `&ctx`, `world`, `tools_called`).
- BudgetExhausted path: after `self.hooks.fire(&Event::SessionEnd, world);` and before `Ok(Outcome::BudgetExhausted { … })`, insert:
  ```rust
        self.run_learning_review(&ctx, world, tools_called).await;
  ```

If the borrow checker complains that `&ctx` conflicts with a later move of `ctx` in the budget path, reorder so the review runs before any `ctx` field is moved (in the budget path `ctx` is not moved into the Outcome, so it's fine).

- [ ] **Step 2: End-to-end fork test**

Create `crates/harness-loop/tests/learning_loop.rs`:

```rust
//! with_learning_loop forks a review subagent at SessionEnd that writes a skill.

use async_trait::async_trait;
use harness_context::default_world;
use harness_core::{Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, Tool, ToolCall, Usage};
use harness_loop::{AgentLoop, LearningConfig};
use harness_tools_skills::SkillManageTool;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const SKILL_MD: &str = "---\nname: learned-skill\ndescription: A thing learned.\n---\n# Learned\n1. do it\n";

fn mi() -> ModelInfo {
    ModelInfo { handle: "mock".into(), provider: "mock".into(), model: "mock".into(), context_window: 8192, input_cost_usd_per_million_tokens: None, output_cost_usd_per_million_tokens: None, supports_tool_use: true, supports_streaming: false }
}

/// Main model: makes 2 tool calls (to a noop tool) then finishes.
struct MainModel { turn: AtomicU32 }
#[async_trait]
impl Model for MainModel {
    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t < 2 {
            Ok(ModelOutput { text: Some("work".into()), tool_calls: vec![ToolCall { id: format!("c{t}"), name: "noop".into(), args: serde_json::json!({}) }], usage: Usage::default(), stop_reason: StopReason::ToolUse, reasoning: None })
        } else {
            Ok(ModelOutput { text: Some("done".into()), tool_calls: vec![], usage: Usage::default(), stop_reason: StopReason::EndTurn, reasoning: None })
        }
    }
    fn info(&self) -> ModelInfo { mi() }
}

/// Review model: emits one skill_manage(create) call then stops.
struct ReviewModel { turn: AtomicU32, fail: bool }
#[async_trait]
impl Model for ReviewModel {
    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        if self.fail { return Err(ModelError::Transport("boom".into())); }
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t == 0 {
            Ok(ModelOutput { text: None, tool_calls: vec![ToolCall { id: "s1".into(), name: "skill_manage".into(), args: serde_json::json!({"action":"create","name":"learned-skill","content": SKILL_MD}) }], usage: Usage::default(), stop_reason: StopReason::ToolUse, reasoning: None })
        } else {
            Ok(ModelOutput { text: Some("reviewed".into()), tool_calls: vec![], usage: Usage::default(), stop_reason: StopReason::EndTurn, reasoning: None })
        }
    }
    fn info(&self) -> ModelInfo { mi() }
}

/// Trivial no-op tool so the main model's calls succeed + increment tools_called.
struct Noop { schema: harness_core::ToolSchema }
impl Noop { fn new() -> Self { Self { schema: harness_core::ToolSchema { name: "noop".into(), description: "noop".into(), input: serde_json::json!({"type":"object"}) } } } }
#[async_trait]
impl Tool for Noop {
    fn name(&self) -> &str { &self.schema.name }
    fn schema(&self) -> &harness_core::ToolSchema { &self.schema }
    fn risk(&self) -> harness_core::ToolRisk { harness_core::ToolRisk::ReadOnly }
    async fn invoke(&self, _a: serde_json::Value, _w: &mut harness_core::World) -> Result<harness_core::ToolResult, harness_core::ToolError> {
        Ok(harness_core::ToolResult { ok: true, content: serde_json::json!({}), trace: None })
    }
}

fn skills_dir() -> std::path::PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("harness-learn-{}-{n}", std::process::id()))
}

fn task() -> harness_core::Task { harness_core::Task { description: "do real work".into(), source: None, deadline: None } }

#[tokio::test]
async fn review_writes_a_skill_after_enough_tool_calls() {
    let dir = skills_dir();
    let review_model: Arc<dyn Model> = Arc::new(ReviewModel { turn: AtomicU32::new(0), fail: false });
    let cfg = LearningConfig::new(review_model)
        .with_tool(Arc::new(SkillManageTool::new(&dir)))
        .with_nudge_interval(2);
    let loop_ = AgentLoop::new(MainModel { turn: AtomicU32::new(0) })
        .with_tool(Arc::new(Noop::new()))
        .with_learning_loop(cfg);

    let mut world = default_world(".");
    let outcome = loop_.run(task(), &mut world).await.unwrap();
    matches!(outcome, harness_loop::Outcome::Done { .. });

    // The review forked and wrote the skill.
    assert!(dir.join("learned-skill").join("SKILL.md").exists(), "review should have written the skill");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn below_threshold_does_not_review() {
    let dir = skills_dir();
    let review_model: Arc<dyn Model> = Arc::new(ReviewModel { turn: AtomicU32::new(0), fail: false });
    let cfg = LearningConfig::new(review_model)
        .with_tool(Arc::new(SkillManageTool::new(&dir)))
        .with_nudge_interval(5); // main model only makes 2 tool calls < 5
    let loop_ = AgentLoop::new(MainModel { turn: AtomicU32::new(0) })
        .with_tool(Arc::new(Noop::new()))
        .with_learning_loop(cfg);
    let mut world = default_world(".");
    let _ = loop_.run(task(), &mut world).await.unwrap();
    assert!(!dir.join("learned-skill").exists(), "no review below threshold");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn review_failure_is_best_effort() {
    let dir = skills_dir();
    let review_model: Arc<dyn Model> = Arc::new(ReviewModel { turn: AtomicU32::new(0), fail: true });
    let cfg = LearningConfig::new(review_model)
        .with_tool(Arc::new(SkillManageTool::new(&dir)))
        .with_nudge_interval(2);
    let loop_ = AgentLoop::new(MainModel { turn: AtomicU32::new(0) })
        .with_tool(Arc::new(Noop::new()))
        .with_learning_loop(cfg);
    let mut world = default_world(".");
    // The run must still succeed even though the review model errors.
    let outcome = loop_.run(task(), &mut world).await;
    assert!(outcome.is_ok(), "review failure must not fail the run");
    let _ = std::fs::remove_dir_all(&dir);
}
```

Add to `crates/harness-loop/Cargo.toml` `[dev-dependencies]`: `harness-tools-skills = { workspace = true }` (+ confirm `harness-context`, `harness-core`, `tokio`, `async-trait`, `serde_json` present). Confirm `ModelError::Other(String)` is the right variant (check `crates/harness-core/src/error.rs` / model.rs); if the variant differs, use the real one. Confirm `ModelInfo::default()` exists; else construct a minimal `ModelInfo`.

- [ ] **Step 3: Run + verify**

Run: `cargo test -p harness-rs-loop --test learning_loop` then `cargo test -p harness-rs-loop`
Expected: 3 learning tests PASS; all existing loop tests still PASS (incl. recall).

- [ ] **Step 4: Commit**

```bash
git add crates/harness-loop/src/lib.rs crates/harness-loop/tests/learning_loop.rs crates/harness-loop/Cargo.toml
git commit -m "feat(harness-loop): fork review subagent at SessionEnd (threshold-gated, best-effort)"
```

---

## Final verification (after all tasks)

- [ ] `cargo build` — clean (the pre-existing `examples/deepseek-hello` break is unrelated).
- [ ] `cargo test -p harness-rs-core -p harness-rs-skills -p harness-rs-tools-skills -p harness-rs-loop` — all green.
- [ ] `cargo tree -p harness-rs-loop | grep -iE "tools-skills|tools-memory|harness-rs-skills"` → these appear only as **dev**-deps of harness-loop, NOT normal deps (the learning loop takes tools by injection; harness-loop core stays decoupled). Verify with `cargo tree -p harness-rs-loop -e normal | grep -iE "tools-skills|tools-memory|rs-skills"` → empty.
- [ ] Dispatch a final code-reviewer over the whole branch.

## Notes for the implementer

- **Decoupling invariant:** harness-loop must NOT gain a *normal* dependency on harness-tools-skills / harness-tools-memory / harness-skills. The white-listed tools arrive via `LearningConfig.tools` (injected by the app). They appear only as harness-loop **dev**-deps (for the test).
- **No review recursion:** the review subagent is a plain `AgentLoop` with no `learning` set, so it can't trigger its own review. Don't set learning on the subagent.
- **Best-effort:** `run_learning_review` swallows the subagent error; a review failure must never change the finished run's `Outcome`.
- **Per-run trigger:** the threshold is on THIS run's `tools_called`. (Cross-run nudge accounting is out of scope.)
- **Commits:** no Co-Authored-By / AI attribution.
