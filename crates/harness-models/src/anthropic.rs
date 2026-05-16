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
    cfg:            LlmConfig,
    client:         reqwest::Client,
    context_window: u32,
    api_version:    String,
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
            api_version:    "2023-06-01".into(),
        }
    }

    pub fn with_context_window(mut self, w: u32) -> Self {
        self.context_window = w;
        self
    }

    pub fn with_api_version(mut self, v: impl Into<String>) -> Self {
        self.api_version = v.into();
        self
    }

    pub fn config(&self) -> &LlmConfig { &self.cfg }
}

// ----------------------------------------------------------------
// Wire format
// ----------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model:      &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system:     Option<String>,
    messages:   Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools:      Vec<AnthropicTool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role:    String, // "user" | "assistant"
    content: Vec<AnthropicBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: JsonValue },
    ToolResult { tool_use_id: String, content: String },
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name:         String,
    description:  String,
    input_schema: JsonValue,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content:      Vec<AnthropicBlock>,
    #[serde(default)]
    stop_reason:  Option<String>,
    #[serde(default)]
    usage:        AnthropicUsage,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens:  u32,
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
                name:         t.name.clone(),
                description:  t.description.clone(),
                input_schema: t.input.clone(),
            })
            .collect();

        let req = AnthropicRequest {
            model:      &self.cfg.model,
            max_tokens: ctx.policy.max_output_tokens.max(1024),
            system,
            messages,
            tools,
        };

        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", &self.api_version)
            .json(&req)
            .send()
            .await
            .map_err(|e| ModelError::Transport(format!("send: {e}")))?;

        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| ModelError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ModelError::Transport(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            )));
        }
        let parsed: AnthropicResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ModelError::Invalid(format!("parse: {e}; body: {}", String::from_utf8_lossy(&bytes))))?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for b in parsed.content {
            match b {
                AnthropicBlock::Text { text: t } => text.push_str(&t),
                AnthropicBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall { id, name, args: input });
                }
                AnthropicBlock::ToolResult { .. } => {} // shouldn't appear in assistant response
            }
        }

        let stop_reason = match parsed.stop_reason.as_deref() {
            Some("end_turn")       => StopReason::EndTurn,
            Some("tool_use")       => StopReason::ToolUse,
            Some("max_tokens")     => StopReason::MaxTokens,
            Some("stop_sequence")  => StopReason::StopSequence,
            _                      => {
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
                input_tokens:        parsed.usage.input_tokens,
                output_tokens:       parsed.usage.output_tokens,
                cached_input_tokens: parsed.usage.cache_read_input_tokens,
            },
            stop_reason,
            // Anthropic returns thinking via separate content blocks; for v0.1
            // we don't surface them in `reasoning` (they round-trip via
            // history if/when we wire `Block::Reasoning → thinking blocks`).
            reasoning: None,
        })
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle:                                  self.cfg.name.clone(),
            provider:                                "anthropic".into(),
            model:                                   self.cfg.model.clone(),
            context_window:                          self.context_window,
            input_cost_usd_per_million_tokens:       None,
            output_cost_usd_per_million_tokens:      None,
            supports_tool_use:                       true,
            supports_streaming:                      false, // not wired yet
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
        };

        let mut blocks = Vec::new();
        for b in &turn.blocks {
            match b {
                Block::Text(s) => {
                    if !s.is_empty() { blocks.push(AnthropicBlock::Text { text: s.clone() }); }
                }
                Block::ToolCall { call_id, name, args } => {
                    blocks.push(AnthropicBlock::ToolUse {
                        id:    call_id.clone(),
                        name:  name.clone(),
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
                        content:     s,
                    });
                }
                Block::FileRef { path, excerpt, .. } => {
                    let mut s = format!("[file:{path}]\n");
                    if let Some(e) = excerpt { s.push_str(e); }
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
                Block::Reasoning(_) => {
                    // Anthropic's thinking blocks have stricter shape; for v0.1
                    // we drop reasoning content rather than risk an invalid request.
                }
            }
        }
        if blocks.is_empty() { continue; }
        // Anthropic requires alternation; merge consecutive same-role messages.
        if let Some(last) = out.last_mut()
            && last.role == role
        {
            last.content.extend(blocks);
        } else {
            out.push(AnthropicMessage { role: role.into(), content: blocks });
        }
    }

    if out.is_empty() {
        out.push(AnthropicMessage {
            role: "user".into(),
            content: vec![AnthropicBlock::Text { text: ctx.task.description.clone() }],
        });
    }

    (system, out)
}
