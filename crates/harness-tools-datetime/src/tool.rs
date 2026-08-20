//! The `resolve_datetime` tool — the agent-facing wrapper over
//! [`crate::resolve`].
//!
//! The library layer never touches a clock; this is the one place where
//! `now` may default to the system's current local time (when the model
//! doesn't pass it). Deployments that need a caller-controlled or
//! user-timezone `now` should have the loop inject it into the arguments.

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, Local};
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};

use crate::{Conventions, resolve_with};

/// Tool: deterministically resolve a natural-language date/time expression
/// to an absolute instant (plus an optional recurrence rule).
///
/// Name: `resolve_datetime` — keep it stable, external evals depend on it.
pub struct ResolveDatetimeTool {
    schema: ToolSchema,
    conventions: Conventions,
}

impl Default for ResolveDatetimeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolveDatetimeTool {
    pub fn new() -> Self {
        Self::with_conventions(Conventions::default())
    }

    /// Override the phrase → time convention table (defaults: 下班前 → 18:00,
    /// 睡前 → 22:00).
    pub fn with_conventions(conventions: Conventions) -> Self {
        Self {
            conventions,
            schema: ToolSchema {
                name: "resolve_datetime".into(),
                description: "Deterministically resolve a natural-language date/time \
                              expression (Chinese or English) to an absolute RFC3339 \
                              instant, plus a recurrence rule when the text describes a \
                              repeating schedule. Rule-based — no model, no network. \
                              Returns resolved=false for expressions it cannot handle \
                              (ambiguous bare hours, time ranges, interval recurrence); \
                              it never guesses. Use it before persisting any user-stated \
                              time like \"明天下午3点\" or \"every Monday at 9am\"."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "The natural-language time expression, e.g. \"下周五下班前\", \"tomorrow at 3pm\", \"每天晚上11点\"."
                        },
                        "now": {
                            "type": "string",
                            "description": "Reference instant, RFC3339 with UTC offset (e.g. \"2026-06-15T09:00:00+08:00\"). The offset also fixes the timezone of the result. Defaults to the host's current local time."
                        }
                    },
                    "required": ["text"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for ResolveDatetimeTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "resolve_datetime".into(),
                reason: "text required".into(),
            })?;
        let now: DateTime<FixedOffset> = match args.get("now").and_then(|v| v.as_str()) {
            Some(s) => {
                DateTime::parse_from_rfc3339(s.trim()).map_err(|e| ToolError::InvalidArgs {
                    name: "resolve_datetime".into(),
                    reason: format!("now must be RFC3339 with offset: {e}"),
                })?
            }
            None => Local::now().fixed_offset(),
        };
        let content = match resolve_with(text, now, &self.conventions) {
            Some(r) => json!({
                "resolved": true,
                "start": r.start.to_rfc3339(),
                "date_only": r.date_only,
                "recurrence": r.recurrence,
                "matched": r.matched,
                "now": now.to_rfc3339(),
            }),
            None => json!({
                "resolved": false,
                "reason": "no deterministic rule matched (ambiguous, out-of-scope, or no \
                           time expression found); caller may fall back to a model",
                "now": now.to_rfc3339(),
            }),
        };
        Ok(ToolResult {
            ok: true,
            content,
            trace: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::RepoView;
    use std::sync::Arc;

    struct NoopRunner;
    #[async_trait]
    impl harness_core::ProcessRunner for NoopRunner {
        async fn exec(
            &self,
            _: &str,
            _: &[&str],
            _: Option<&std::path::Path>,
        ) -> std::io::Result<harness_core::ProcessOutput> {
            Ok(harness_core::ProcessOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
    struct NoopClock;
    impl harness_core::Clock for NoopClock {
        fn now_ms(&self) -> i64 {
            0
        }
    }
    struct NoopKv;
    #[async_trait]
    impl harness_core::KvStore for NoopKv {
        async fn get(&self, _: &str) -> Option<Vec<u8>> {
            None
        }
        async fn set(&self, _: &str, _: Vec<u8>) {}
        async fn delete(&self, _: &str) {}
    }

    fn world() -> World {
        World {
            repo: RepoView {
                root: std::env::temp_dir(),
            },
            runner: Arc::new(NoopRunner),
            clock: Arc::new(NoopClock),
            kv: Arc::new(NoopKv),
            profile: harness_core::UserProfile::default(),
            session: None,
        }
    }

    #[tokio::test]
    async fn resolves_with_explicit_now() {
        let tool = ResolveDatetimeTool::new();
        let out = tool
            .invoke(
                json!({"text": "明天下午3点开会", "now": "2026-06-15T09:00:00+08:00"}),
                &mut world(),
            )
            .await
            .unwrap();
        assert!(out.ok);
        assert_eq!(out.content["resolved"], true);
        assert_eq!(out.content["start"], "2026-06-16T15:00:00+08:00");
        assert_eq!(out.content["date_only"], false);
    }

    #[tokio::test]
    async fn recurrence_is_reported() {
        let tool = ResolveDatetimeTool::new();
        let out = tool
            .invoke(
                json!({"text": "every Monday at 9am", "now": "2026-06-15T09:00:00+08:00"}),
                &mut world(),
            )
            .await
            .unwrap();
        assert_eq!(out.content["resolved"], true);
        assert_eq!(out.content["recurrence"]["freq"], "weekly");
        assert_eq!(out.content["recurrence"]["days"], json!([1]));
        // 9am has already struck this Monday — first occurrence is next week.
        assert_eq!(out.content["start"], "2026-06-22T09:00:00+08:00");
    }

    #[tokio::test]
    async fn unsupported_is_honest_not_guessed() {
        let tool = ResolveDatetimeTool::new();
        let out = tool
            .invoke(
                json!({"text": "每隔20分钟喝口水", "now": "2026-06-15T09:00:00+08:00"}),
                &mut world(),
            )
            .await
            .unwrap();
        assert!(out.ok);
        assert_eq!(out.content["resolved"], false);
    }

    #[tokio::test]
    async fn missing_text_is_invalid_args() {
        let tool = ResolveDatetimeTool::new();
        let err = tool.invoke(json!({}), &mut world()).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn bad_now_is_invalid_args() {
        let tool = ResolveDatetimeTool::new();
        let err = tool
            .invoke(json!({"text": "明天", "now": "yesterday"}), &mut world())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn now_defaults_to_system_time() {
        let tool = ResolveDatetimeTool::new();
        let out = tool
            .invoke(json!({"text": "明天"}), &mut world())
            .await
            .unwrap();
        assert_eq!(out.content["resolved"], true);
        assert_eq!(out.content["date_only"], true);
    }
}
