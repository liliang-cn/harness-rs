//! The hashline tools: `hash_read` (view a file with content anchors) and
//! `hash_edit` (edit by quoting those anchors).

use crate::hashline;
use crate::jail::resolve;
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::json;
use std::sync::OnceLock;

/// `hash_read` — read a file as a hashline view (`HHHH  <code>` per line).
pub struct HashRead;
static HASH_READ_SCHEMA: OnceLock<ToolSchema> = OnceLock::new();

#[async_trait]
impl Tool for HashRead {
    fn name(&self) -> &str {
        "hash_read"
    }
    fn schema(&self) -> &ToolSchema {
        HASH_READ_SCHEMA.get_or_init(|| ToolSchema {
            name: "hash_read".into(),
            description: "Read a text file as a HASHLINE view. Each line is returned as \
                          `HHHH  <code>`, where HHHH is a stable 4-char anchor derived from \
                          the line's content. Edit by quoting these anchors in `hash_edit`. \
                          The anchors are NOT part of the file."
                .into(),
            input: json!({
                "type": "object",
                "properties": { "path": {"type": "string", "description": "Path relative to the workspace root."} },
                "required": ["path"]
            }),
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let path = args["path"].as_str().unwrap_or_default();
        let abs = resolve(&world.repo.root, path)?;
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| ToolError::Exec(format!("{}: {e}", abs.display())))?;
        let view = hashline::render(&content);
        Ok(ToolResult {
            ok: true,
            content: json!({ "path": path, "lines": view.lines().count(), "view": view }),
            trace: None,
        })
    }
}

/// `hash_edit` — apply hashline operations anchored by content hash.
pub struct HashEdit;
static HASH_EDIT_SCHEMA: OnceLock<ToolSchema> = OnceLock::new();

#[async_trait]
impl Tool for HashEdit {
    fn name(&self) -> &str {
        "hash_edit"
    }
    fn schema(&self) -> &ToolSchema {
        HASH_EDIT_SCHEMA.get_or_init(|| ToolSchema {
            name: "hash_edit".into(),
            description: "Edit a file by HASHLINE anchors (from hash_read). Give a list of ops, \
                          each quoting a 4-char anchor. Anchors identify lines by content, not \
                          number, so a batch of edits is safe regardless of line shifts. `text` \
                          is the new line content (omit for delete); it may contain \\n. Returns \
                          the refreshed hashline view."
                .into(),
            input: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op":     {"type": "string", "enum": ["replace", "insert_after", "insert_before", "delete"]},
                                "anchor": {"type": "string", "description": "4-char content anchor from hash_read"},
                                "text":   {"type": "string", "description": "New content (omit for delete)."}
                            },
                            "required": ["op", "anchor"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Destructive
    }
    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let path = args["path"].as_str().unwrap_or_default();
        let abs = resolve(&world.repo.root, path)?;
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| ToolError::Exec(format!("{}: {e}", abs.display())))?;

        let raw = args["edits"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "hash_edit".into(),
                reason: "`edits` must be an array".into(),
            })?;
        let mut edits = Vec::with_capacity(raw.len());
        for e in raw {
            let op = hashline::Op::parse(e["op"].as_str().unwrap_or("")).ok_or_else(|| {
                ToolError::InvalidArgs {
                    name: "hash_edit".into(),
                    reason: format!("bad op: {}", e["op"]),
                }
            })?;
            edits.push(hashline::Edit {
                op,
                anchor: e["anchor"].as_str().unwrap_or_default().to_string(),
                text: e["text"].as_str().map(|s| s.to_string()),
            });
        }

        let updated = hashline::apply(&content, &edits).map_err(ToolError::Exec)?;
        tokio::fs::write(&abs, &updated)
            .await
            .map_err(|e| ToolError::Exec(format!("write {}: {e}", abs.display())))?;
        Ok(ToolResult {
            ok: true,
            content: json!({
                "path": path,
                "applied": edits.len(),
                "view": hashline::render(&updated),
            }),
            trace: None,
        })
    }
}
