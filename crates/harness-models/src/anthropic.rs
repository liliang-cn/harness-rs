//! Anthropic Messages API adapter.
//!
//! Endpoint: `POST <base_url>/v1/messages`
//! Auth:     `x-api-key: <api_key>` + `anthropic-version: 2023-06-01`
//!
//! Translates the framework's generic `Context` (with `tools` and history) into
//! Anthropic's content-block message format.

use crate::LlmConfig;
use async_trait::async_trait;
use harness_core::{
    Block, Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, ToolCall, TurnRole,
    Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::time::Duration;

pub struct AnthropicNative {
    cfg: LlmConfig,
    client: reqwest::Client,
    context_window: u32,
    api_version: String,
}

impl AnthropicNative {
    pub fn new(cfg: LlmConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client builds");
        Self {
            cfg,
            client,
            context_window: 200_000,
            api_version: "2023-06-01".into(),
        }
    }

    /// Convenience: pass model name + API key. URL is api.anthropic.com.
    ///
    /// ```ignore
    /// use harness_models::AnthropicNative;
    /// let m = AnthropicNative::with_key("claude-opus-4-7", api_key);
    /// ```
    pub fn with_key(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let model = model.into();
        Self::new(LlmConfig::new(
            format!("anthropic:{model}"),
            crate::providers::ANTHROPIC,
            api_key,
            model,
        ))
    }

    pub fn with_context_window(mut self, w: u32) -> Self {
        self.context_window = w;
        self
    }

    pub fn with_api_version(mut self, v: impl Into<String>) -> Self {
        self.api_version = v.into();
        self
    }

    pub fn config(&self) -> &LlmConfig {
        &self.cfg
    }
}

// ----------------------------------------------------------------
// Wire format
// ----------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String, // "user" | "assistant"
    content: Vec<AnthropicBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    /// Extended thinking block. Required to be echoed back verbatim to the API
    /// (with signature) on subsequent calls during a thinking conversation.
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Redacted thinking — content opaque, must still be passed through.
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: JsonValue,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

// ----------------------------------------------------------------
// Model impl
// ----------------------------------------------------------------

#[async_trait]
impl Model for AnthropicNative {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let (system, messages) = build_messages(ctx);
        let tools = ctx
            .tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input.clone(),
            })
            .collect();

        let req = AnthropicRequest {
            model: &self.cfg.model,
            max_tokens: ctx.policy.max_output_tokens.max(1024),
            system,
            messages,
            tools,
        };

        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let bytes = crate::retry::with_retry("anthropic:complete", || async {
            let resp = self
                .client
                .post(&url)
                .header("x-api-key", &self.cfg.api_key)
                .header("anthropic-version", &self.api_version)
                .json(&req)
                .send()
                .await
                .map_err(|e| crate::retry::Retryable::transient(format!("send: {e}")))?;
            let status = resp.status();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| crate::retry::Retryable::transient(format!("body: {e}")))?;
            if !status.is_success() {
                let body = String::from_utf8_lossy(&bytes).to_string();
                let msg = format!("HTTP {status}: {body}");
                return Err(
                    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        crate::retry::Retryable::transient(msg)
                    } else {
                        crate::retry::Retryable::permanent(msg)
                    },
                );
            }
            Ok(bytes)
        })
        .await
        .map_err(ModelError::Transport)?;
        let parsed: AnthropicResponse = serde_json::from_slice(&bytes).map_err(|e| {
            ModelError::Invalid(format!(
                "parse: {e}; body: {}",
                String::from_utf8_lossy(&bytes)
            ))
        })?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut reasoning = String::new();
        for b in parsed.content {
            match b {
                AnthropicBlock::Text { text: t } => text.push_str(&t),
                AnthropicBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        args: input,
                    });
                }
                AnthropicBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    // Round-trip via Block::Reasoning. Signature is required by
                    // Anthropic when echoing back — pack it as JSON.
                    let packed = serde_json::json!({
                        "kind": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    });
                    if !reasoning.is_empty() {
                        reasoning.push('\n');
                    }
                    reasoning.push_str(&packed.to_string());
                }
                AnthropicBlock::RedactedThinking { data } => {
                    let packed = serde_json::json!({
                        "kind": "redacted_thinking",
                        "data": data,
                    });
                    if !reasoning.is_empty() {
                        reasoning.push('\n');
                    }
                    reasoning.push_str(&packed.to_string());
                }
                AnthropicBlock::ToolResult { .. } => {} // shouldn't appear in assistant response
            }
        }

        let stop_reason = match parsed.stop_reason.as_deref() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            _ => {
                if !tool_calls.is_empty() {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }
            }
        };

        Ok(ModelOutput {
            text: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            usage: Usage {
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
                cached_input_tokens: parsed.usage.cache_read_input_tokens,
            },
            stop_reason,
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
        })
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle: self.cfg.name.clone(),
            provider: "anthropic".into(),
            model: self.cfg.model.clone(),
            context_window: self.context_window,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: true,
            supports_streaming: false, // not wired yet
        }
    }
}

fn build_messages(ctx: &Context) -> (Option<String>, Vec<AnthropicMessage>) {
    // System: concat system + guide blocks.
    let mut system_buf = String::new();
    for b in ctx.system.iter().chain(ctx.guides.iter()) {
        if let Block::Text(s) = b {
            system_buf.push_str(s);
            system_buf.push('\n');
        }
    }
    let system = if system_buf.trim().is_empty() {
        None
    } else {
        Some(system_buf)
    };

    // Translate turns.
    let mut out: Vec<AnthropicMessage> = Vec::new();
    for turn in &ctx.history {
        let role = match turn.role {
            TurnRole::User => "user",
            TurnRole::Assistant => "assistant",
            TurnRole::Tool => "user", // Anthropic models tool results as user-role with tool_result blocks
            TurnRole::System => continue, // already consumed above
            _ => "user",              // forward-compat: unknown roles fall back to user
        };

        let mut blocks = Vec::new();
        for b in &turn.blocks {
            match b {
                Block::Text(s) => {
                    if !s.is_empty() {
                        blocks.push(AnthropicBlock::Text { text: s.clone() });
                    }
                }
                Block::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    blocks.push(AnthropicBlock::ToolUse {
                        id: call_id.clone(),
                        name: name.clone(),
                        input: args.clone(),
                    });
                }
                Block::ToolResult { call_id, content } => {
                    let s = match content {
                        JsonValue::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    blocks.push(AnthropicBlock::ToolResult {
                        tool_use_id: call_id.clone(),
                        content: s,
                    });
                }
                Block::FileRef { path, excerpt, .. } => {
                    let mut s = format!("[file:{path}]\n");
                    if let Some(e) = excerpt {
                        s.push_str(e);
                    }
                    blocks.push(AnthropicBlock::Text { text: s });
                }
                Block::Skill { name, body } => {
                    blocks.push(AnthropicBlock::Text {
                        text: format!("[skill:{name}]\n{body}"),
                    });
                }
                Block::Feedback(signals) => {
                    for s in signals {
                        blocks.push(AnthropicBlock::Text {
                            text: format!(
                                "[feedback:{}] {}",
                                s.origin,
                                s.agent_hint.as_deref().unwrap_or(&s.message)
                            ),
                        });
                    }
                }
                Block::Reasoning(raw) => {
                    // `Block::Reasoning` was packed by the inbound parser as one
                    // JSON object per line: {"kind":"thinking","thinking":..,
                    // "signature":..} or {"kind":"redacted_thinking","data":..}.
                    // Restore the exact wire shape so Anthropic accepts the echo.
                    for line in raw.lines() {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let Ok(v) = serde_json::from_str::<JsonValue>(line) else {
                            continue;
                        };
                        match v.get("kind").and_then(|k| k.as_str()) {
                            Some("thinking") => {
                                if let Some(t) = v.get("thinking").and_then(|x| x.as_str()) {
                                    blocks.push(AnthropicBlock::Thinking {
                                        thinking: t.to_string(),
                                        signature: v
                                            .get("signature")
                                            .and_then(|x| x.as_str())
                                            .map(str::to_string),
                                    });
                                }
                            }
                            Some("redacted_thinking") => {
                                if let Some(d) = v.get("data").and_then(|x| x.as_str()) {
                                    blocks.push(AnthropicBlock::RedactedThinking {
                                        data: d.to_string(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {} // forward-compat: unknown Block variants silently skipped
            }
        }
        if blocks.is_empty() {
            continue;
        }
        // Anthropic requires alternation; merge consecutive same-role messages.
        if let Some(last) = out.last_mut()
            && last.role == role
        {
            last.content.extend(blocks);
        } else {
            out.push(AnthropicMessage {
                role: role.into(),
                content: blocks,
            });
        }
    }

    if out.is_empty() {
        out.push(AnthropicMessage {
            role: "user".into(),
            content: vec![AnthropicBlock::Text {
                text: ctx.task.description.clone(),
            }],
        });
    }

    (system, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Block, Policy, Task, Turn, TurnRole};
    use std::collections::BTreeMap;

    fn empty_ctx() -> Context {
        Context {
            system: vec![Block::Text("be helpful".into())],
            guides: vec![Block::Text("be terse".into())],
            history: vec![],
            task: Task {
                description: "do the thing".into(),
                source: None,
                deadline: None,
            },
            policy: Policy::default(),
            metadata: BTreeMap::new(),
            tools: vec![],
        }
    }

    #[test]
    fn build_messages_concatenates_system_and_falls_back_to_task() {
        let (system, msgs) = build_messages(&empty_ctx());
        assert!(system.unwrap().contains("be helpful"));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        match &msgs[0].content[0] {
            AnthropicBlock::Text { text } => assert_eq!(text, "do the thing"),
            other => panic!("unexpected block: {other:?}"),
        }
    }

    #[test]
    fn build_messages_translates_tool_calls_and_results() {
        let mut ctx = empty_ctx();
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text("read it".into())],
        });
        ctx.history.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::ToolCall {
                call_id: "c1".into(),
                name: "read_file".into(),
                args: serde_json::json!({"path": "x"}),
            }],
        });
        ctx.history.push(Turn {
            role: TurnRole::Tool,
            blocks: vec![Block::ToolResult {
                call_id: "c1".into(),
                content: serde_json::json!("hello"),
            }],
        });
        let (_system, msgs) = build_messages(&ctx);
        // user -> assistant(tool_use) -> user(tool_result)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert!(matches!(msgs[1].content[0], AnthropicBlock::ToolUse { .. }));
        assert_eq!(msgs[2].role, "user");
        assert!(matches!(
            msgs[2].content[0],
            AnthropicBlock::ToolResult { .. }
        ));
    }

    #[test]
    fn reasoning_block_round_trips_through_wire_format() {
        let mut ctx = empty_ctx();
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text("think".into())],
        });
        // Simulate a previous assistant turn carrying packed thinking.
        let packed = serde_json::json!({
            "kind": "thinking",
            "thinking": "I should consider X",
            "signature": "sig123"
        })
        .to_string();
        ctx.history.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::Reasoning(packed), Block::Text("therefore Y".into())],
        });
        let (_system, msgs) = build_messages(&ctx);
        let assistant = msgs.iter().find(|m| m.role == "assistant").unwrap();
        let has_thinking = assistant.content.iter().any(|b| {
            matches!(
                b,
                AnthropicBlock::Thinking { thinking, signature: Some(s) }
                    if thinking == "I should consider X" && s == "sig123"
            )
        });
        assert!(
            has_thinking,
            "thinking block missing in echo: {:#?}",
            assistant.content
        );
        let has_text = assistant
            .content
            .iter()
            .any(|b| matches!(b, AnthropicBlock::Text { text } if text.contains("therefore Y")));
        assert!(has_text);
    }

    #[test]
    fn redacted_thinking_also_round_trips() {
        let mut ctx = empty_ctx();
        let packed = serde_json::json!({
            "kind": "redacted_thinking",
            "data": "OPAQUE_BLOB"
        })
        .to_string();
        ctx.history.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::Reasoning(packed)],
        });
        let (_system, msgs) = build_messages(&ctx);
        let assistant = msgs.iter().find(|m| m.role == "assistant").unwrap();
        assert!(matches!(
            assistant.content[0],
            AnthropicBlock::RedactedThinking { ref data } if data == "OPAQUE_BLOB"
        ));
    }
}
