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

    /// Convenience: `base_url + model + api_key`. You pass the endpoint —
    /// nothing is hardcoded.
    ///
    /// ```ignore
    /// use harness_models::AnthropicNative;
    /// let m = AnthropicNative::with_key("https://api.anthropic.com", "claude-opus-4-7", api_key);
    /// ```
    pub fn with_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let model = model.into();
        Self::new(LlmConfig::new(
            format!("anthropic:{model}"),
            base_url,
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
    /// Structured blocks rather than a bare string: Anthropic's prompt cache is
    /// opt-in per block, so the breakpoint has nowhere to live on a plain
    /// `String`. See [`SystemBlock`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<SystemBlock>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
}

/// One system block, optionally marked as the end of the cacheable prefix.
///
/// Unlike DeepSeek-style automatic prefix caching, Anthropic only caches what is
/// explicitly marked: without a `cache_control` breakpoint the system prompt and
/// tool schemas are re-read at full price on every turn of every run. They are
/// also the largest fixed cost in a loop — the same bytes, resent each iteration.
#[derive(Debug, Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: &'static str, // "text"
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// `{"type": "ephemeral"}` — Anthropic's ~5-minute prompt cache.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".into(),
        }
    }
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Extended thinking block. Required to be echoed back verbatim to the API
    /// (with signature) on subsequent calls during a thinking conversation.
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Redacted thinking — content opaque, must still be passed through.
    RedactedThinking { data: String },
    /// Inline image (vision). `{"type":"image","source":{"type":"base64",...}}`.
    Image {
        source: AnthropicImageSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    kind: String, // "base64"
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: JsonValue,
    /// Set on the *last* tool only: the breakpoint caches everything before it,
    /// and Anthropic allows a small number of breakpoints per request.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
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
    /// Tokens written into the cache on this call — what the first turn pays to
    /// make the later ones cheap.
    #[serde(default)]
    cache_creation_input_tokens: u32,
}

// ----------------------------------------------------------------
// Model impl
// ----------------------------------------------------------------

#[async_trait]
impl Model for AnthropicNative {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let (system, mut messages) = build_messages(ctx);
        // Second breakpoint: end of the conversation. See mark_history_breakpoint.
        mark_history_breakpoint(&mut messages);
        // Anthropic has no native `response_format` field as of Dec 2025, and
        // their `tool_choice` forced-tool trick conflicts with real
        // tool-using loops (forcing one tool blocks the model from calling
        // any others). Best-effort approach: append the schema/JSON
        // instruction to the system prompt and trust the model to follow.
        let system = augment_system_for_response_format(system, &ctx.response_format);
        let mut tools: Vec<AnthropicTool> = ctx
            .tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input.clone(),
                cache_control: None,
            })
            .collect();
        // One breakpoint at the end of the prefix — system + every tool schema.
        // Those bytes are identical on every iteration of a run and on every run
        // of a long-lived service; unmarked, Anthropic re-reads them at full
        // price each time. Marking the last tool covers the tools *and* the
        // system block before them, which is why the system block itself is left
        // unmarked when tools are present.
        let cache_on_system = if let Some(last) = tools.last_mut() {
            last.cache_control = Some(CacheControl::ephemeral());
            false
        } else {
            true
        };

        let req = AnthropicRequest {
            model: &self.cfg.model,
            max_tokens: ctx.policy.max_output_tokens.max(1024),
            system: system_blocks(system, cache_on_system),
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
                AnthropicBlock::Text { text: t, .. } => text.push_str(&t),
                AnthropicBlock::ToolUse {
                    id, name, input, ..
                } => {
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
                AnthropicBlock::ToolResult { .. } | AnthropicBlock::Image { .. } => {} // not in assistant responses
            }
        }

        if parsed.usage.cache_creation_input_tokens > 0 || parsed.usage.cache_read_input_tokens > 0
        {
            tracing::debug!(
                target: "harness.models",
                cache_creation_input_tokens = parsed.usage.cache_creation_input_tokens,
                cache_read_input_tokens = parsed.usage.cache_read_input_tokens,
                "anthropic prompt cache"
            );
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
                cache_write_input_tokens: parsed.usage.cache_creation_input_tokens,
            },
            stop_reason,
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            // Anthropic's Messages API has no image-output channel today.
            // Deferred deliberately: shipping untested parsing for a shape we
            // cannot exercise is worse than not shipping it.
            images: Vec::new(),
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
            supports_streaming: false,
            supports_web_grounding: false, // not wired yet
        }
    }
}

/// Best-effort response-format support for Anthropic: append schema/JSON
/// instructions to the system prompt. Anthropic doesn't ship a native
/// `response_format` parameter; the documented alternatives (forced
/// tool_choice, response prefill) both break the multi-turn ReAct loop in
/// non-trivial ways, so we fall back to prompt steering.
fn augment_system_for_response_format(
    system: Option<String>,
    fmt: &harness_core::ResponseFormat,
) -> Option<String> {
    use harness_core::ResponseFormat;
    let extra = match fmt {
        ResponseFormat::Free => return system,
        ResponseFormat::JsonObject => {
            "Reply with valid JSON only — no markdown fences, no prose, no explanation.".to_string()
        }
        ResponseFormat::JsonSchema { schema, .. } => format!(
            "Reply ONLY with a single JSON object that matches this schema (no markdown fences, no prose):\n{}",
            serde_json::to_string(schema).unwrap_or_else(|_| "{}".into())
        ),
        // ResponseFormat is `#[non_exhaustive]`; unknown future variants ⇒
        // free-form (no prompt injection).
        _ => return system,
    };
    Some(match system {
        Some(s) if !s.trim().is_empty() => format!("{s}\n\n{extra}"),
        _ => extra,
    })
}

/// Wrap the system prompt as a block list, marking it as the cacheable prefix
/// when nothing later in the request carries the breakpoint.
fn system_blocks(system: Option<String>, cache: bool) -> Vec<SystemBlock> {
    match system {
        Some(text) if !text.trim().is_empty() => vec![SystemBlock {
            kind: "text",
            text,
            cache_control: cache.then(CacheControl::ephemeral),
        }],
        _ => Vec::new(),
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
                        blocks.push(AnthropicBlock::Text {
                            text: s.clone(),
                            cache_control: None,
                        });
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
                        cache_control: None,
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
                        cache_control: None,
                    });
                }
                Block::Image { media_type, base64 } => {
                    blocks.push(AnthropicBlock::Image {
                        source: AnthropicImageSource {
                            kind: "base64".into(),
                            media_type: media_type.clone(),
                            data: base64.clone(),
                        },
                        cache_control: None,
                    });
                }
                Block::FileRef { path, excerpt, .. } => {
                    let mut s = format!("[file:{path}]\n");
                    if let Some(e) = excerpt {
                        s.push_str(e);
                    }
                    blocks.push(AnthropicBlock::Text {
                        text: s,
                        cache_control: None,
                    });
                }
                Block::Skill { name, body } => {
                    blocks.push(AnthropicBlock::Text {
                        text: format!("[skill:{name}]\n{body}"),
                        cache_control: None,
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
                            cache_control: None,
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
                cache_control: None,
            }],
        });
    }

    (system, out)
}

/// Mark the last cache-capable block of the conversation as the end of the
/// cacheable prefix — the second breakpoint, after the system/tools one.
///
/// An agent loop appends to its history and resends it whole; without this
/// breakpoint every iteration re-reads the entire conversation at full price,
/// and after a few tool calls the history dwarfs the static prefix. Marking
/// the final block makes this turn's request next turn's cache hit: Anthropic
/// matches the longest previously-cached prefix, so only the newly appended
/// blocks are paid at write price.
///
/// Thinking blocks cannot carry `cache_control`, so walk backwards to the
/// nearest block that can.
fn mark_history_breakpoint(messages: &mut [AnthropicMessage]) {
    for msg in messages.iter_mut().rev() {
        for block in msg.content.iter_mut().rev() {
            let slot = match block {
                AnthropicBlock::Text { cache_control, .. }
                | AnthropicBlock::ToolUse { cache_control, .. }
                | AnthropicBlock::ToolResult { cache_control, .. }
                | AnthropicBlock::Image { cache_control, .. } => cache_control,
                AnthropicBlock::Thinking { .. } | AnthropicBlock::RedactedThinking { .. } => {
                    continue;
                }
            };
            *slot = Some(CacheControl::ephemeral());
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Block, Policy, Task, Turn, TurnRole};
    use std::collections::BTreeMap;

    pub(super) fn empty_ctx() -> Context {
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
            response_format: harness_core::ResponseFormat::Free,
        }
    }

    #[test]
    fn build_messages_concatenates_system_and_falls_back_to_task() {
        let (system, msgs) = build_messages(&empty_ctx());
        assert!(system.unwrap().contains("be helpful"));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        match &msgs[0].content[0] {
            AnthropicBlock::Text { text, .. } => assert_eq!(text, "do the thing"),
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
        let has_text = assistant.content.iter().any(
            |b| matches!(b, AnthropicBlock::Text { text, .. } if text.contains("therefore Y")),
        );
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

#[cfg(test)]
mod cache_tests {
    use super::tests::empty_ctx;
    use super::*;
    use harness_core::{Block, Turn, TurnRole};

    fn tool(name: &str) -> AnthropicTool {
        AnthropicTool {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,
        }
    }

    /// The prefix — system prompt plus every tool schema — is byte-identical on
    /// every turn, and Anthropic only caches what a breakpoint marks. One
    /// breakpoint on the last tool covers the whole prefix.
    #[test]
    fn the_last_tool_carries_the_breakpoint() {
        let mut tools = vec![tool("read_file"), tool("list_dir")];
        let cache_on_system = if let Some(last) = tools.last_mut() {
            last.cache_control = Some(CacheControl::ephemeral());
            false
        } else {
            true
        };
        assert!(
            !cache_on_system,
            "tools present → system needs no breakpoint"
        );

        let req = AnthropicRequest {
            model: "claude-x",
            max_tokens: 1024,
            system: system_blocks(Some("be helpful".into()), cache_on_system),
            messages: vec![],
            tools,
        };
        let v = serde_json::to_value(&req).unwrap();

        assert!(
            v["tools"][0].get("cache_control").is_none(),
            "only the final tool is marked: {v}"
        );
        assert_eq!(v["tools"][1]["cache_control"]["type"], "ephemeral", "{v}");
        assert!(
            v["system"][0].get("cache_control").is_none(),
            "the tool breakpoint already covers the system block: {v}"
        );
    }

    /// With no tools there is nothing after the system prompt to carry the mark,
    /// so it goes on the system block itself — otherwise a tool-less agent
    /// (a summariser, a classifier) never caches at all.
    #[test]
    fn without_tools_the_system_block_is_marked() {
        let blocks = system_blocks(Some("be helpful".into()), true);
        let v = serde_json::to_value(&blocks).unwrap();
        assert_eq!(v[0]["type"], "text");
        assert_eq!(v[0]["cache_control"]["type"], "ephemeral", "{v}");
    }

    /// An empty system prompt must not produce an empty block: Anthropic rejects
    /// a blank `text`, and `system` is skipped entirely when the list is empty.
    #[test]
    fn empty_system_produces_no_block() {
        assert!(system_blocks(None, true).is_empty());
        assert!(system_blocks(Some("   ".into()), true).is_empty());
    }

    /// Count blocks carrying a breakpoint across all messages.
    fn marked_blocks(msgs: &[AnthropicMessage]) -> usize {
        let v = serde_json::to_value(msgs).unwrap();
        v.as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().unwrap())
            .filter(|b| b.get("cache_control").is_some())
            .count()
    }

    /// The history breakpoint lands on the final block of the final message —
    /// exactly one mark, so this turn's whole request becomes next turn's
    /// cached prefix.
    #[test]
    fn the_last_history_block_carries_the_breakpoint() {
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
        let (_system, mut msgs) = build_messages(&ctx);
        mark_history_breakpoint(&mut msgs);

        assert_eq!(marked_blocks(&msgs), 1, "exactly one history breakpoint");
        let v = serde_json::to_value(&msgs).unwrap();
        let last = v.as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["cache_control"]["type"], "ephemeral", "{v}");
    }

    /// Thinking blocks cannot carry `cache_control`; when the conversation ends
    /// in one, the mark walks back to the nearest block that can.
    #[test]
    fn history_breakpoint_skips_a_thinking_tail() {
        let mut msgs = vec![AnthropicMessage {
            role: "assistant".into(),
            content: vec![
                AnthropicBlock::Text {
                    text: "so far".into(),
                    cache_control: None,
                },
                AnthropicBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: None,
                },
            ],
        }];
        mark_history_breakpoint(&mut msgs);

        let v = serde_json::to_value(&msgs).unwrap();
        assert!(
            v[0]["content"][1].get("cache_control").is_none(),
            "thinking must stay unmarked: {v}"
        );
        assert_eq!(
            v[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "{v}"
        );
    }

    /// A conversation with nothing markable (all thinking) must not panic and
    /// must not mark anything.
    #[test]
    fn history_breakpoint_handles_unmarkable_history() {
        let mut msgs = vec![AnthropicMessage {
            role: "assistant".into(),
            content: vec![AnthropicBlock::RedactedThinking { data: "x".into() }],
        }];
        mark_history_breakpoint(&mut msgs);
        assert_eq!(marked_blocks(&msgs), 0);

        let mut empty: Vec<AnthropicMessage> = vec![];
        mark_history_breakpoint(&mut empty);
    }
}
