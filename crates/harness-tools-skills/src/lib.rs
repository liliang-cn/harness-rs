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
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("harness-skillmanage-{}-{n}", std::process::id()))
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
}
