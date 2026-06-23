//! `skill_manage` — the LLM-facing tool that lets an agent author its own skills
//! (create/patch/edit/delete SKILL.md). State-bearing (holds the skills dir), so
//! constructed at wiring time like `RememberThisTool`. Risk = Destructive.

use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
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
                    patch (replace old_string->new_string in a skill), delete. \
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
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Destructive
    }

    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let action = Self::arg(&args, "action").ok_or_else(|| ToolError::InvalidArgs {
            name: "skill_manage".into(),
            reason: "action required".into(),
        })?;
        let name = Self::arg(&args, "name").ok_or_else(|| ToolError::InvalidArgs {
            name: "skill_manage".into(),
            reason: "name required".into(),
        })?;

        // Validate the skill name up front, before any filesystem access, so
        // EVERY action is path-safe. In particular `patch` used to `join(name)`
        // and read the file *before* validation, letting a crafted name like
        // `../other/skill` read outside the skills dir (an existence-probe leak
        // in multi-tenant hosts). create/edit/delete already validate inside
        // write_skill_md/delete_skill; this also covers patch and is harmless
        // defense-in-depth for the rest.
        if let Err(e) = harness_skills::validate_name(name) {
            return Ok(ToolResult {
                ok: false,
                content: json!({"error": e.to_string()}),
                trace: None,
            });
        }

        let result: Result<Value, String> = match action {
            "create" | "edit" => {
                let content =
                    Self::arg(&args, "content").ok_or_else(|| ToolError::InvalidArgs {
                        name: "skill_manage".into(),
                        reason: "content required for create/edit".into(),
                    })?;
                harness_skills::write_skill_md(&self.dir, name, content)
                    .map(|p| json!({"action": action, "name": name, "path": p.to_string_lossy()}))
                    .map_err(|e| e.to_string())
            }
            "patch" => {
                let old = Self::arg(&args, "old_string").ok_or_else(|| ToolError::InvalidArgs {
                    name: "skill_manage".into(),
                    reason: "old_string required for patch".into(),
                })?;
                let new = Self::arg(&args, "new_string").unwrap_or("");
                let path = self.dir.join(name).join("SKILL.md");
                match std::fs::read_to_string(&path) {
                    Ok(cur) => {
                        let matches = cur.matches(old).count();
                        if matches == 0 {
                            Err(format!("old_string not found in {name}"))
                        } else if matches > 1 {
                            Err(format!(
                                "old_string not unique in {name} ({matches} matches)"
                            ))
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
            "delete" => harness_skills::delete_skill(&self.dir, name)
                .map(|removed| json!({"action": "delete", "name": name, "removed": removed}))
                .map_err(|e| e.to_string()),
            other => Err(format!("unknown action `{other}`")),
        };

        match result {
            Ok(content) => Ok(ToolResult {
                ok: true,
                content,
                trace: None,
            }),
            Err(reason) => Ok(ToolResult {
                ok: false,
                content: json!({"error": reason}),
                trace: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::default_world;

    fn tmp() -> PathBuf {
        // A per-call atomic counter, NOT just the clock: macOS `SystemTime` is
        // coarse enough that two tests in this binary running concurrently could
        // get the same nanos → the same dir, and one test's end-of-run
        // `remove_dir_all` would then yank the other's files mid-test (flaky
        // `exists()` failures under `cargo test --workspace`).
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "harness-skillmanage-{}-{n}-{seq}",
            std::process::id()
        ))
    }

    const SKILL: &str =
        "---\nname: deploy-runbook\ndescription: How to deploy.\n---\n# Deploy\n1. build\n";

    #[tokio::test]
    async fn create_patch_delete() {
        let dir = tmp();
        let tool = SkillManageTool::new(&dir);
        let mut w = default_world(".");

        let out = tool
            .invoke(
                json!({"action":"create","name":"deploy-runbook","content": SKILL}),
                &mut w,
            )
            .await
            .unwrap();
        assert!(out.ok, "create: {:?}", out.content);
        assert!(dir.join("deploy-runbook").join("SKILL.md").exists());

        let out = tool.invoke(json!({"action":"patch","name":"deploy-runbook","old_string":"1. build","new_string":"1. build\n2. test"}), &mut w).await.unwrap();
        assert!(out.ok, "patch: {:?}", out.content);
        let body = std::fs::read_to_string(dir.join("deploy-runbook").join("SKILL.md")).unwrap();
        assert!(body.contains("2. test"));

        let out = tool
            .invoke(json!({"action":"delete","name":"deploy-runbook"}), &mut w)
            .await
            .unwrap();
        assert!(out.ok);
        assert!(!dir.join("deploy-runbook").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bad_name_returns_ok_false() {
        let dir = tmp();
        let tool = SkillManageTool::new(&dir);
        let mut w = default_world(".");
        let out = tool
            .invoke(
                json!({"action":"create","name":"Bad Name","content": SKILL}),
                &mut w,
            )
            .await
            .unwrap();
        assert!(!out.ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `patch` must reject a path-traversal name BEFORE touching the filesystem,
    /// so it can't read a SKILL.md outside the tool's own skills dir.
    #[tokio::test]
    async fn patch_rejects_traversal_name() {
        // Plant a "victim" skill in a sibling dir of the tool's skills root.
        let base = tmp();
        let tool_dir = base.join("attacker");
        let victim_dir = base.join("victim");
        std::fs::create_dir_all(victim_dir.join("secret-skill")).unwrap();
        std::fs::write(victim_dir.join("secret-skill").join("SKILL.md"), SKILL).unwrap();

        let tool = SkillManageTool::new(&tool_dir);
        let mut w = default_world(".");
        // ../victim/secret-skill would escape `attacker/` into `victim/`.
        let out = tool
            .invoke(
                json!({"action":"patch","name":"../victim/secret-skill",
                       "old_string":"1. build","new_string":"x"}),
                &mut w,
            )
            .await
            .unwrap();
        assert!(
            !out.ok,
            "traversal name must be rejected, got: {:?}",
            out.content
        );
        // The victim file must be untouched.
        let body =
            std::fs::read_to_string(victim_dir.join("secret-skill").join("SKILL.md")).unwrap();
        assert_eq!(body, SKILL);
        let _ = std::fs::remove_dir_all(&base);
    }
}
