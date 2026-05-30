//! Google Gemini native API adapter.
//!
//! Endpoint: `POST <base_url>/v1beta/models/<model>:generateContent`
//! Auth:     `x-goog-api-key: <api_key>` header (or `?key=` query string).
//!
//! Compared to going through the OpenAI-compat shim this adapter:
//! 1. **Carries `thoughtSignature` round-trip** across tool-call cycles —
//!    required by Gemini 3.x for tool use to work at all.
//! 2. **Enables Google Search grounding** when `with_search_grounding(true)`,
//!    so Gemini answers with up-to-date facts without the agent ever calling a
//!    DDG/Bing-style HTML scraper. Critical when running from an IP that the
//!    public search engines have blacklisted.
//!
//! The framework's tool registry is still passed via the `tools[].functionDeclarations`
//! channel; google_search lives in a parallel `tools[].googleSearch` slot and
//! Gemini decides on its own when to use it.
//!
//! Signatures are stashed in `Block::Reasoning` as one JSON line per
//! tool call: `{"kind":"gemini_sig","call_id":"...","signature":"..."}`,
//! then matched back by call_id when sending the next request.

use crate::LlmConfig;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use harness_core::{
    Block, Context, Model, ModelDelta, ModelError, ModelInfo, ModelOutput, StopReason, ToolCall,
    TurnRole, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

pub struct GeminiNative {
    cfg: LlmConfig,
    client: reqwest::Client,
    context_window: u32,
    enable_search_grounding: bool,
}

impl GeminiNative {
    pub fn new(cfg: LlmConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client builds");
        Self {
            cfg,
            client,
            context_window: 1_000_000, // Gemini 2.5+ is 1M context
            enable_search_grounding: true,
        }
    }

    /// `model` is the Gemini model id (e.g. `gemini-3.5-flash`, `gemini-2.5-pro`).
    /// Base URL defaults to `https://generativelanguage.googleapis.com`.
    pub fn with_key(model: impl Into<String>, api_key: impl Into<String>) -> Self {
        let model = model.into();
        Self::new(LlmConfig::new(
            format!("gemini:{model}"),
            crate::providers::GEMINI,
            api_key,
            model,
        ))
    }

    /// Toggle Google Search grounding. On by default.
    /// When on, Gemini may pull live web facts into its reply without
    /// dispatching a function-call back to the agent loop.
    pub fn with_search_grounding(mut self, enabled: bool) -> Self {
        self.enable_search_grounding = enabled;
        self
    }

    pub fn config(&self) -> &LlmConfig {
        &self.cfg
    }
}

// ───── Wire format ─────

#[derive(Serialize)]
struct GeminiRequest<'a> {
    contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiTool>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing)]
    _life: std::marker::PhantomData<&'a ()>,
}

#[derive(Serialize, Default)]
struct GeminiToolConfig {
    /// Required by Gemini 3.x when `googleSearch` and `functionDeclarations`
    /// coexist in `tools[]` (otherwise: HTTP 400 INVALID_ARGUMENT).
    #[serde(
        rename = "includeServerSideToolInvocations",
        skip_serializing_if = "Option::is_none"
    )]
    include_server_side_tool_invocations: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GeminiContent {
    role: String, // "user" | "model"
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
enum GeminiPart {
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
        /// SIBLING of `functionCall` (per
        /// https://ai.google.dev/gemini-api/docs/thought-signatures). Echo
        /// EXACTLY in the same part on subsequent requests or Gemini 3.x
        /// returns HTTP 400.
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    /// Server-side built-in tool invocation (Google Search, code execution).
    /// Gemini executes these itself; the agent loop just observes the queries.
    ServerToolCall {
        #[serde(rename = "toolCall")]
        tool_call: ServerToolCallPayload,
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    /// Server-side built-in tool response (mirror of the call above).
    ServerToolResponse {
        #[serde(rename = "toolResponse")]
        tool_response: serde_json::Value,
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    Text {
        text: String,
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    /// Catch-all: any other part shape Gemini may emit (forward-compat). The
    /// raw JSON is preserved but not interpreted.
    Other(serde_json::Value),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ServerToolCallPayload {
    /// Gemini 3.x: required to round-trip when echoing the model's previous turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "toolType")]
    tool_type: String,
    #[serde(default)]
    args: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GeminiFunctionCall {
    /// Gemini 3.x parallel-call identifier. Must round-trip in subsequent
    /// requests when echoing the model's previous turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    name: String,
    #[serde(default)]
    args: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GeminiFunctionResponse {
    /// Must match the originating `functionCall.id` (Gemini 3.x).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    name: String,
    response: JsonValue,
}

#[derive(Serialize)]
struct GeminiSystem {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GeminiTool {
    Functions {
        #[serde(rename = "functionDeclarations")]
        function_declarations: Vec<GeminiFunctionDecl>,
    },
    GoogleSearch {
        #[serde(rename = "googleSearch")]
        google_search: Map<String, JsonValue>,
    },
}

#[derive(Serialize)]
struct GeminiFunctionDecl {
    name: String,
    description: String,
    parameters: JsonValue,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    /// Set to `"application/json"` to force JSON output. Required when
    /// `response_schema` is set; also useful on its own for `JsonObject` mode.
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<String>,
    /// Schema the JSON output must match. Gemini's schema dialect is a
    /// subset of JSON Schema 2020-12: no `$ref`, no `oneOf` at top level,
    /// `additionalProperties` ignored, etc. We pass the schema through
    /// unchanged and surface any 400 errors back to the caller.
    #[serde(rename = "responseSchema", skip_serializing_if = "Option::is_none")]
    response_schema: Option<JsonValue>,
}

#[derive(Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: GeminiUsage,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
    #[serde(rename = "groundingMetadata", default)]
    grounding_metadata: Option<JsonValue>,
}

#[derive(Default, Deserialize)]
struct GeminiUsage {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
}

// ───── Model impl ─────

#[async_trait]
impl Model for GeminiNative {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let (system, contents) = build_contents(ctx);

        let mut tools: Vec<GeminiTool> = Vec::new();
        if !ctx.tools.is_empty() {
            tools.push(GeminiTool::Functions {
                function_declarations: ctx
                    .tools
                    .iter()
                    .map(|t| GeminiFunctionDecl {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.input.clone(),
                    })
                    .collect(),
            });
        }
        if self.enable_search_grounding {
            tools.push(GeminiTool::GoogleSearch {
                google_search: Map::new(),
            });
        }

        // Gemini 3.x: must enable server-side tool invocations when
        // googleSearch + function declarations are both requested.
        let mixed_tools = self.enable_search_grounding && !ctx.tools.is_empty();
        let tool_config = if mixed_tools {
            Some(GeminiToolConfig {
                include_server_side_tool_invocations: Some(true),
            })
        } else {
            None
        };

        let req = GeminiRequest {
            contents,
            system_instruction: system.map(|t| GeminiSystem {
                parts: vec![GeminiPart::Text {
                    text: t,
                    thought_signature: None,
                }],
            }),
            tools,
            tool_config,
            generation_config: Some({
                let (mime, schema) = gemini_response_format(&ctx.response_format);
                GeminiGenerationConfig {
                    max_output_tokens: Some(ctx.policy.max_output_tokens.max(1024)),
                    response_mime_type: mime,
                    response_schema: schema,
                }
            }),
            _life: std::marker::PhantomData,
        };

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.cfg.base_url.trim_end_matches('/'),
            self.cfg.model
        );
        if std::env::var("HARNESS_DUMP_GEMINI_REQ").is_ok()
            && let Ok(j) = serde_json::to_string_pretty(&req)
        {
            eprintln!("=== gemini request ===\n{j}\n======================");
        }
        let bytes = crate::retry::with_retry("gemini:complete", || async {
            let resp = self
                .client
                .post(&url)
                .header("x-goog-api-key", &self.cfg.api_key)
                .header("Content-Type", "application/json")
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

        let parsed: GeminiResponse = serde_json::from_slice(&bytes).map_err(|e| {
            ModelError::Invalid(format!(
                "parse: {e}; body: {}",
                String::from_utf8_lossy(&bytes)
            ))
        })?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        let mut sig_lines: Vec<String> = Vec::new();
        let mut grounding_chunks: Vec<JsonValue> = Vec::new();

        let mut finish_reason: Option<String> = None;
        for cand in parsed.candidates {
            finish_reason = cand.finish_reason.clone();
            if let Some(meta) = cand.grounding_metadata {
                grounding_chunks.push(meta);
            }
            let Some(content) = cand.content else {
                continue;
            };
            // Patch: inject a synthesized `id` into any FunctionCall part that
            // lacks one. Gemini 3.x requires the id to be present when this
            // turn is echoed back, even if the model didn't emit one.
            let mut patched_parts: Vec<GeminiPart> = Vec::with_capacity(content.parts.len());
            for part in content.parts {
                match part {
                    GeminiPart::FunctionCall {
                        function_call: fc,
                        thought_signature,
                    } => {
                        let call_id = fc.id.clone().unwrap_or_else(|| {
                            format!("gemini-call-{}-{}", fc.name, tool_calls.len() as u32 + 1)
                        });
                        tool_calls.push(ToolCall {
                            id: call_id.clone(),
                            name: fc.name.clone(),
                            args: fc.args.clone(),
                        });
                        patched_parts.push(GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                id: Some(call_id),
                                name: fc.name,
                                args: fc.args,
                            },
                            thought_signature,
                        });
                    }
                    GeminiPart::Text {
                        text: t,
                        thought_signature,
                    } => {
                        text.push_str(&t);
                        patched_parts.push(GeminiPart::Text {
                            text: t,
                            thought_signature,
                        });
                    }
                    other => patched_parts.push(other),
                }
            }
            // Pack the (now-patched) parts verbatim into reasoning so the next
            // request can echo it back without losing signatures, ids, or
            // server-side tool chains.
            if let Ok(parts_json) = serde_json::to_value(&patched_parts) {
                sig_lines.push(
                    serde_json::json!({
                        "kind": "gemini_parts",
                        "parts": parts_json,
                    })
                    .to_string(),
                );
            }
        }

        // Surface grounding info as an extra reasoning line so downstream
        // observability (SessionRecorder, LiveProgressHook) can see it.
        if !grounding_chunks.is_empty() {
            sig_lines.push(
                serde_json::json!({
                    "kind": "gemini_grounding",
                    "metadata": grounding_chunks,
                })
                .to_string(),
            );
        }

        let stop_reason = match finish_reason.as_deref() {
            Some("STOP") => StopReason::EndTurn,
            Some("MAX_TOKENS") => StopReason::MaxTokens,
            Some("TOOL_USE") | Some("TOOL_CALL") => StopReason::ToolUse,
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
                input_tokens: parsed.usage_metadata.prompt_token_count,
                output_tokens: parsed.usage_metadata.candidates_token_count,
                cached_input_tokens: 0,
            },
            stop_reason,
            reasoning: if sig_lines.is_empty() {
                None
            } else {
                Some(sig_lines.join("\n"))
            },
        })
    }

    async fn stream(
        &self,
        ctx: &Context,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let (system, contents) = build_contents(ctx);
        let mut tools: Vec<GeminiTool> = Vec::new();
        if !ctx.tools.is_empty() {
            tools.push(GeminiTool::Functions {
                function_declarations: ctx
                    .tools
                    .iter()
                    .map(|t| GeminiFunctionDecl {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.input.clone(),
                    })
                    .collect(),
            });
        }
        if self.enable_search_grounding {
            tools.push(GeminiTool::GoogleSearch {
                google_search: Map::new(),
            });
        }
        let mixed_tools = self.enable_search_grounding && !ctx.tools.is_empty();
        let tool_config = if mixed_tools {
            Some(GeminiToolConfig {
                include_server_side_tool_invocations: Some(true),
            })
        } else {
            None
        };
        let req = GeminiRequest {
            contents,
            system_instruction: system.map(|t| GeminiSystem {
                parts: vec![GeminiPart::Text {
                    text: t,
                    thought_signature: None,
                }],
            }),
            tools,
            tool_config,
            generation_config: Some({
                let (mime, schema) = gemini_response_format(&ctx.response_format);
                GeminiGenerationConfig {
                    max_output_tokens: Some(ctx.policy.max_output_tokens.max(1024)),
                    response_mime_type: mime,
                    response_schema: schema,
                }
            }),
            _life: std::marker::PhantomData,
        };
        // `alt=sse` makes Gemini return `data: { ... }\n\n`-framed SSE events
        // instead of the default newline-delimited JSON. Same payload shape
        // per event as the non-stream `GeminiResponse`, but `candidates[*]`
        // streams in fragments and `usageMetadata` only appears on the
        // final chunk.
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.cfg.base_url.trim_end_matches('/'),
            self.cfg.model
        );
        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.cfg.api_key)
            .header("Content-Type", "application/json")
            .json(&req)
            .send()
            .await
            .map_err(|e| ModelError::Transport(format!("send: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ModelError::Transport(format!("HTTP {status}: {body}")));
        }
        let byte_stream = resp.bytes_stream();
        Ok(parse_gemini_sse(byte_stream).boxed())
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle: self.cfg.name.clone(),
            provider: "gemini".into(),
            model: self.cfg.model.clone(),
            context_window: self.context_window,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: true,
            supports_streaming: true,
        }
    }
}

// ───── Context → Gemini contents ─────

/// Translate `Context.response_format` into Gemini's
/// `generationConfig.{responseMimeType, responseSchema}` pair.
///
/// - `Free` ⇒ both `None`; Gemini returns prose.
/// - `JsonObject` ⇒ mime set, schema `None`; Gemini returns JSON of any shape.
/// - `JsonSchema` ⇒ both set; schema sanitised for Gemini's dialect (which
///   is OpenAPI-ish, NOT JSON Schema). See [`sanitize_for_gemini`] for what
///   we strip / inline.
fn gemini_response_format(
    fmt: &harness_core::ResponseFormat,
) -> (Option<String>, Option<JsonValue>) {
    use harness_core::ResponseFormat;
    match fmt {
        ResponseFormat::Free => (None, None),
        ResponseFormat::JsonObject => (Some("application/json".into()), None),
        ResponseFormat::JsonSchema { schema, .. } => (
            Some("application/json".into()),
            Some(sanitize_for_gemini(schema)),
        ),
        // ResponseFormat is `#[non_exhaustive]`; unknown future variants get
        // free-form fallback.
        _ => (None, None),
    }
}

/// Per-stream state for the Gemini SSE parser. Lives at module scope so
/// `process_gemini_chunk` can borrow it mutably without leaking the
/// upstream byte-stream type parameter into a trait.
struct GeminiStreamState<S> {
    upstream: S,
    buf: String,
    eof: bool,
    pending: VecDeque<Result<ModelDelta, ModelError>>,
    raw_parts: Vec<JsonValue>,
    grounding_chunks: Vec<JsonValue>,
    tool_call_seq: u32,
    sent_reasoning: bool,
}

/// Parse Gemini's `:streamGenerateContent?alt=sse` byte stream into
/// `ModelDelta`s. Each SSE event carries a full `GeminiResponse`-shaped
/// chunk; we crack it open and emit one delta per useful piece — text
/// fragment, function call, usage, finish reason. The raw `parts[]` JSON
/// is collected across all chunks and emitted once at the end as a
/// `ModelDelta::Reasoning` blob so the next turn can echo it back
/// verbatim (preserving `thoughtSignature`s — required by Gemini 3.x).
fn parse_gemini_sse<S>(stream: S) -> impl futures::Stream<Item = Result<ModelDelta, ModelError>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    use futures::stream::unfold;

    let init = GeminiStreamState {
        upstream: stream,
        buf: String::new(),
        eof: false,
        pending: VecDeque::new(),
        raw_parts: Vec::new(),
        grounding_chunks: Vec::new(),
        tool_call_seq: 0,
        sent_reasoning: false,
    };

    unfold(init, |mut state| async move {
        if let Some(d) = state.pending.pop_front() {
            return Some((d, state));
        }
        if state.eof {
            return None;
        }
        loop {
            if let Some(sep) = state.buf.find("\n\n") {
                let event = state.buf.drain(..sep + 2).collect::<String>();
                let payload = event
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
                    .collect::<Vec<_>>()
                    .join("");
                if payload.is_empty() {
                    continue;
                }
                let v: GeminiResponse = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                process_gemini_chunk(&mut state, v);
                if let Some(d) = state.pending.pop_front() {
                    return Some((d, state));
                }
                continue;
            }
            match state.upstream.next().await {
                Some(Ok(bytes)) => {
                    if let Ok(s) = std::str::from_utf8(&bytes) {
                        // Gemini ships SSE with CRLF line endings (`\r\n\r\n`
                        // between events). Normalise to LF so the rest of
                        // this parser can treat events as plain `\n\n`-
                        // delimited.
                        if s.contains('\r') {
                            state
                                .buf
                                .push_str(&s.replace("\r\n", "\n").replace('\r', "\n"));
                        } else {
                            state.buf.push_str(s);
                        }
                    } else {
                        tracing::warn!(len = bytes.len(), "gemini bytes chunk not utf-8");
                    }
                }
                Some(Err(e)) => {
                    state.eof = true;
                    return Some((Err(ModelError::Transport(format!("stream: {e}"))), state));
                }
                None => {
                    if !state.buf.trim().is_empty() {
                        let payload = state
                            .buf
                            .lines()
                            .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
                            .collect::<Vec<_>>()
                            .join("");
                        state.buf.clear();
                        if !payload.is_empty()
                            && let Ok(v) = serde_json::from_str::<GeminiResponse>(&payload)
                        {
                            process_gemini_chunk(&mut state, v);
                        }
                    }
                    if !state.sent_reasoning {
                        state.sent_reasoning = true;
                        if let Some(blob) = build_reasoning_blob(&state) {
                            state.pending.push_back(Ok(ModelDelta::Reasoning(blob)));
                        }
                    }
                    state.eof = true;
                    return state.pending.pop_front().map(|d| (d, state));
                }
            }
        }
    })
}

fn process_gemini_chunk<S>(state: &mut GeminiStreamState<S>, chunk: GeminiResponse) {
    for cand in chunk.candidates {
        if let Some(meta) = cand.grounding_metadata {
            state.grounding_chunks.push(meta);
        }
        if let Some(content) = cand.content {
            for part in content.parts {
                if let Ok(part_json) = serde_json::to_value(&part) {
                    state.raw_parts.push(part_json);
                }
                match &part {
                    GeminiPart::Text { text, .. } if !text.is_empty() => {
                        state.pending.push_back(Ok(ModelDelta::Text(text.clone())));
                    }
                    GeminiPart::FunctionCall {
                        function_call: fc, ..
                    } => {
                        state.tool_call_seq += 1;
                        let call_id = fc.id.clone().unwrap_or_else(|| {
                            format!("gemini-call-{}-{}", fc.name, state.tool_call_seq)
                        });
                        state.pending.push_back(Ok(ModelDelta::ToolCallStart {
                            id: call_id.clone(),
                            name: fc.name.clone(),
                        }));
                        let args_str =
                            serde_json::to_string(&fc.args).unwrap_or_else(|_| "{}".into());
                        state.pending.push_back(Ok(ModelDelta::ToolCallArgs {
                            id: call_id,
                            partial_json: args_str,
                        }));
                    }
                    _ => {}
                }
            }
        }
        if let Some(fr) = cand.finish_reason {
            if !state.sent_reasoning {
                state.sent_reasoning = true;
                if let Some(blob) = build_reasoning_blob(state) {
                    state.pending.push_back(Ok(ModelDelta::Reasoning(blob)));
                }
            }
            let stop = match fr.as_str() {
                "STOP" => StopReason::EndTurn,
                "MAX_TOKENS" => StopReason::MaxTokens,
                "TOOL_USE" | "TOOL_CALL" => StopReason::ToolUse,
                _ => StopReason::EndTurn,
            };
            state.pending.push_back(Ok(ModelDelta::Stop(stop)));
        }
    }
    if chunk.usage_metadata.prompt_token_count > 0
        || chunk.usage_metadata.candidates_token_count > 0
    {
        state.pending.push_back(Ok(ModelDelta::Usage(Usage {
            input_tokens: chunk.usage_metadata.prompt_token_count,
            output_tokens: chunk.usage_metadata.candidates_token_count,
            cached_input_tokens: 0,
        })));
    }
}

fn build_reasoning_blob<S>(state: &GeminiStreamState<S>) -> Option<String> {
    let mut lines = Vec::new();
    if !state.raw_parts.is_empty() {
        lines.push(
            serde_json::json!({
                "kind": "gemini_parts",
                "parts": &state.raw_parts,
            })
            .to_string(),
        );
    }
    if !state.grounding_chunks.is_empty() {
        lines.push(
            serde_json::json!({
                "kind": "gemini_grounding",
                "metadata": &state.grounding_chunks,
            })
            .to_string(),
        );
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Sanitise a JSON Schema for Gemini's responseSchema field.
///
/// Gemini accepts an OpenAPI-3.0-style subset of JSON Schema; the rest of the
/// world's tooling (incl. `schemars`) emits modern JSON Schema with `$ref`,
/// `definitions`, `$defs`, `$schema`, etc. We:
/// 1. inline every `$ref` against the schema's `definitions` / `$defs` table,
/// 2. drop the metadata keys (`$schema`, `definitions`, `$defs`),
/// 3. drop `additionalProperties` (Gemini rejects unknown keys here).
///
/// Recursion depth is bounded by the input schema's nesting — we don't
/// detect ref cycles, but the typical use case (a flat struct via
/// `schemars::schema_for!(T)`) doesn't have them.
fn sanitize_for_gemini(schema: &JsonValue) -> JsonValue {
    let defs = schema
        .get("definitions")
        .or_else(|| schema.get("$defs"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    strip_and_inline(schema, &defs)
}

fn strip_and_inline(v: &JsonValue, defs: &serde_json::Map<String, JsonValue>) -> JsonValue {
    match v {
        JsonValue::Object(o) => {
            if let Some(r) = o.get("$ref").and_then(|s| s.as_str())
                && let Some(name) = r
                    .strip_prefix("#/definitions/")
                    .or_else(|| r.strip_prefix("#/$defs/"))
                && let Some(referent) = defs.get(name)
            {
                return strip_and_inline(referent, defs);
            }
            let mut result = serde_json::Map::new();
            for (k, v2) in o {
                if matches!(
                    k.as_str(),
                    "$schema" | "definitions" | "$defs" | "$ref" | "additionalProperties"
                ) {
                    continue;
                }
                result.insert(k.clone(), strip_and_inline(v2, defs));
            }
            JsonValue::Object(result)
        }
        JsonValue::Array(a) => {
            JsonValue::Array(a.iter().map(|v| strip_and_inline(v, defs)).collect())
        }
        _ => v.clone(),
    }
}

fn build_contents(ctx: &Context) -> (Option<String>, Vec<GeminiContent>) {
    // System: concat system + guide text blocks into one instruction.
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

    // Per-turn parts-replay map: when an assistant turn has a Block::Reasoning
    // line of kind "gemini_parts", we replay that parts array verbatim
    // instead of reconstructing from Block::ToolCall (which would lose
    // thoughtSignature, server-side toolCall/Response, etc).
    let mut raw_parts_by_turn: HashMap<usize, Vec<GeminiPart>> = HashMap::new();
    // call_id → tool name lookup so parallel functionResponse parts can be
    // labelled correctly (Gemini requires id+name to match the originating
    // functionCall).
    let mut tool_name_by_call_id: HashMap<String, String> = HashMap::new();
    for (idx, turn) in ctx.history.iter().enumerate() {
        for b in &turn.blocks {
            match b {
                Block::Reasoning(raw) => {
                    for line in raw.lines() {
                        let Ok(v) = serde_json::from_str::<JsonValue>(line.trim()) else {
                            continue;
                        };
                        if v.get("kind").and_then(|k| k.as_str()) == Some("gemini_parts")
                            && let Some(arr) = v.get("parts")
                            && let Ok(parts) =
                                serde_json::from_value::<Vec<GeminiPart>>(arr.clone())
                        {
                            raw_parts_by_turn.insert(idx, parts);
                        }
                    }
                }
                Block::ToolCall {
                    call_id,
                    name,
                    args: _,
                } => {
                    tool_name_by_call_id.insert(call_id.clone(), name.clone());
                }
                _ => {}
            }
        }
    }

    // Translate turns.
    let mut out: Vec<GeminiContent> = Vec::new();
    for (idx, turn) in ctx.history.iter().enumerate() {
        let role = match turn.role {
            TurnRole::User => "user",
            TurnRole::Assistant => "model",
            TurnRole::Tool => "user", // function responses are sent as user-role parts
            TurnRole::System => continue,
            _ => "user",
        };

        // Verbatim replay path: if we recorded the original parts array on
        // the way in, echo it back unchanged. This preserves signatures and
        // server-side tool chains exactly as Gemini sent them.
        if role == "model"
            && let Some(parts) = raw_parts_by_turn.remove(&idx)
        {
            if let Some(last) = out.last_mut()
                && last.role == role
            {
                last.parts.extend(parts);
            } else {
                out.push(GeminiContent {
                    role: role.into(),
                    parts,
                });
            }
            continue;
        }

        let mut parts: Vec<GeminiPart> = Vec::new();
        for b in &turn.blocks {
            match b {
                Block::Text(s) => {
                    if !s.is_empty() {
                        parts.push(GeminiPart::Text {
                            text: s.clone(),
                            thought_signature: None,
                        });
                    }
                }
                Block::ToolCall {
                    call_id,
                    name,
                    args,
                } => {
                    // Reached only when the model turn has NO `gemini_parts`
                    // reasoning entry (i.e. ToolCall arrived from elsewhere).
                    // Without a stored signature we can't satisfy Gemini 3.x,
                    // but for 2.x and as a defensive fallback we still emit
                    // the call without signature.
                    parts.push(GeminiPart::FunctionCall {
                        function_call: GeminiFunctionCall {
                            id: Some(call_id.clone()),
                            name: name.clone(),
                            args: args.clone(),
                        },
                        thought_signature: None,
                    });
                }
                Block::ToolResult { call_id, content } => {
                    let name = tool_name_by_call_id
                        .get(call_id)
                        .cloned()
                        .unwrap_or_else(|| "tool".into());
                    let response_obj = match content {
                        JsonValue::Object(_) => content.clone(),
                        other => serde_json::json!({"output": other}),
                    };
                    parts.push(GeminiPart::FunctionResponse {
                        function_response: GeminiFunctionResponse {
                            id: Some(call_id.clone()),
                            name,
                            response: response_obj,
                        },
                        thought_signature: None,
                    });
                }
                Block::FileRef { path, excerpt, .. } => {
                    let mut s = format!("[file:{path}]\n");
                    if let Some(e) = excerpt {
                        s.push_str(e);
                    }
                    parts.push(GeminiPart::Text {
                        text: s,
                        thought_signature: None,
                    });
                }
                Block::Skill { name, body } => {
                    parts.push(GeminiPart::Text {
                        text: format!("[skill:{name}]\n{body}"),
                        thought_signature: None,
                    });
                }
                Block::Feedback(signals) => {
                    for s in signals {
                        parts.push(GeminiPart::Text {
                            text: format!(
                                "[feedback:{}] {}",
                                s.origin,
                                s.agent_hint.as_deref().unwrap_or(&s.message)
                            ),
                            thought_signature: None,
                        });
                    }
                }
                Block::Reasoning(_) => { /* handled via raw_parts_by_turn */ }
                _ => {} // forward-compat
            }
        }
        if parts.is_empty() {
            continue;
        }
        // Gemini wants strict user/model alternation; merge consecutive same-role.
        if let Some(last) = out.last_mut()
            && last.role == role
        {
            last.parts.extend(parts);
        } else {
            out.push(GeminiContent {
                role: role.into(),
                parts,
            });
        }
    }

    if out.is_empty() {
        out.push(GeminiContent {
            role: "user".into(),
            parts: vec![GeminiPart::Text {
                text: ctx.task.description.clone(),
                thought_signature: None,
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
            system: vec![],
            guides: vec![],
            history: vec![],
            task: Task {
                description: "hi".into(),
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
    fn empty_ctx_falls_back_to_task_text() {
        let (system, contents) = build_contents(&empty_ctx());
        assert!(system.is_none());
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn raw_parts_replay_preserves_signature_verbatim() {
        let mut ctx = empty_ctx();
        // Note: thoughtSignature is a SIBLING of functionCall in the wire shape,
        // per https://ai.google.dev/gemini-api/docs/thought-signatures.
        let raw = serde_json::json!({
            "kind": "gemini_parts",
            "parts": [
                {
                    "functionCall": {"name": "current_time", "args": {}},
                    "thoughtSignature": "SIG"
                }
            ],
        })
        .to_string();
        ctx.history.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![
                Block::Reasoning(raw),
                Block::ToolCall {
                    call_id: "gemini-call-current_time-1".into(),
                    name: "current_time".into(),
                    args: serde_json::json!({}),
                },
            ],
        });
        let (_, contents) = build_contents(&ctx);
        let model_turn = &contents[0];
        assert_eq!(model_turn.role, "model");
        let sig = model_turn
            .parts
            .iter()
            .find_map(|p| match p {
                GeminiPart::FunctionCall {
                    function_call: _,
                    thought_signature,
                } => thought_signature.clone(),
                _ => None,
            })
            .expect("signature present on functionCall part");
        assert_eq!(sig, "SIG");
    }

    #[test]
    fn system_and_guides_merge_into_instruction() {
        let mut ctx = empty_ctx();
        ctx.system.push(Block::Text("you are a bot".into()));
        ctx.guides.push(Block::Text("be terse".into()));
        let (system, _) = build_contents(&ctx);
        let s = system.unwrap();
        assert!(s.contains("you are a bot"));
        assert!(s.contains("be terse"));
    }

    #[test]
    fn alternating_roles_merge_consecutive_same_role() {
        let mut ctx = empty_ctx();
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text("a".into())],
        });
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text("b".into())],
        });
        let (_, contents) = build_contents(&ctx);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts.len(), 2);
    }
}
