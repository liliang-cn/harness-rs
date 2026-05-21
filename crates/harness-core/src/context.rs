use crate::{ModelOutput, Signal};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single block of content within the assembled prompt.
///
/// Blocks are grouped so that long-stable prefixes (system + guides) stay
/// cacheable across turns ("prompt caching" pattern).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Block {
    /// Plain prompt text.
    Text(String),
    /// Reference to a file in the world. The runtime decides whether to
    /// inline contents or hand the agent a tool call to read it.
    FileRef {
        path: String,
        hash: Option<String>,
        excerpt: Option<String>,
    },
    /// Reference to an activated SKILL.md body.
    Skill { name: String, body: String },
    /// A tool call the assistant requested.
    ToolCall {
        call_id: String,
        name: String,
        args: serde_json::Value,
    },
    /// The result of a previous tool call.
    ToolResult {
        call_id: String,
        content: serde_json::Value,
    },
    /// Feedback signals from sensors, rendered for the model.
    Feedback(Vec<Signal>),
    /// Provider-specific reasoning trace (DeepSeek `reasoning_content`,
    /// Anthropic `thinking` blocks). Must be echoed back to the provider on
    /// subsequent calls or the API rejects the request.
    Reasoning(String),
}

/// A single conversation turn (assistant or user).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: TurnRole,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TurnRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub description: String,
    pub source: Option<String>, // slack url, github issue, etc.
    pub deadline: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Policy {
    pub max_iters: u32,
    pub max_input_tokens: u32,
    pub max_output_tokens: u32,
    pub self_correct_rounds: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_iters: 50,
            max_input_tokens: 150_000,
            max_output_tokens: 8_000,
            self_correct_rounds: 3,
        }
    }
}

/// Constrain the model's terminal (non-tool-call) reply shape. Default = Free.
///
/// Each model adapter translates this to the provider's native format on the
/// wire:
/// - OpenAI / DeepSeek: `response_format: {type: "json_object"}` for
///   `JsonObject`; `{type: "json_schema", json_schema: {name, schema, strict}}`
///   for `JsonSchema`. Providers that only support `json_object` (DeepSeek as
///   of Dec 2025) degrade gracefully by injecting the schema into the system
///   prompt instead.
/// - Gemini: `generationConfig.responseMimeType = "application/json"` plus
///   `generationConfig.responseSchema = <schema>` for `JsonSchema`.
/// - Anthropic: no native field — adapters synthesise a "structured_output"
///   tool with the schema, force `tool_choice` to it, and surface the tool's
///   args as the assistant text on response.
///
/// `JsonSchema.schema` is a `serde_json::Value` so callers can build it
/// however they like — hand-rolled, via `schemars::schema_for!(T)`, or pulled
/// from a `harness_loop::AgentLoop::run_typed<T>()` derivation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResponseFormat {
    /// Free-form text. The framework adds nothing to the request body.
    #[default]
    Free,
    /// "Reply with valid JSON of any shape." Useful when the caller will run
    /// its own validation and doesn't want to commit to a schema yet.
    JsonObject,
    /// "Reply with JSON matching this schema." Adapters may need to sanitise
    /// dialect-specific keys before emitting (Gemini rejects `$ref`, OpenAI
    /// strict mode demands `additionalProperties: false` everywhere, …).
    JsonSchema {
        /// Short identifier — providers that require one (OpenAI) use it as
        /// the `json_schema.name` field.
        name: String,
        /// JSON Schema, as a `serde_json::Value`.
        schema: serde_json::Value,
    },
}

/// The model-visible state of an in-progress agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub system: Vec<Block>,
    pub guides: Vec<Block>,
    pub history: Vec<Turn>,
    pub task: Task,
    pub policy: Policy,
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// Tools the agent may call this turn. Model adapters translate these to
    /// the provider's tool-calling format (OpenAI `tools`, Anthropic `tools`, …).
    pub tools: Vec<crate::ToolSchema>,
    /// Constraint on the model's terminal reply. Defaults to `Free` —
    /// providers receive no extra request fields. See [`ResponseFormat`].
    #[serde(default, skip_serializing_if = "response_format_is_default")]
    pub response_format: ResponseFormat,
}

fn response_format_is_default(f: &ResponseFormat) -> bool {
    matches!(f, ResponseFormat::Free)
}

impl Context {
    pub fn new(task: Task) -> Self {
        Self {
            system: Vec::new(),
            guides: Vec::new(),
            history: Vec::new(),
            task,
            policy: Policy::default(),
            metadata: BTreeMap::new(),
            tools: Vec::new(),
            response_format: ResponseFormat::Free,
        }
    }

    /// Append a model turn to the history. Captures reasoning content so it
    /// can be echoed back on subsequent calls (required by DeepSeek thinking
    /// mode and Anthropic thinking blocks).
    pub fn push_model_output(&mut self, out: &ModelOutput) {
        let mut blocks = Vec::new();
        if let Some(r) = &out.reasoning
            && !r.is_empty()
        {
            blocks.push(Block::Reasoning(r.clone()));
        }
        if let Some(t) = &out.text
            && !t.is_empty()
        {
            blocks.push(Block::Text(t.clone()));
        }
        for c in &out.tool_calls {
            blocks.push(Block::ToolCall {
                call_id: c.id.clone(),
                name: c.name.clone(),
                args: c.args.clone(),
            });
        }
        self.history.push(Turn {
            role: TurnRole::Assistant,
            blocks,
        });
    }

    /// Append feedback signals as a tool-role turn.
    pub fn push_feedback(&mut self, signals: Vec<Signal>) {
        self.history.push(Turn {
            role: TurnRole::Tool,
            blocks: vec![Block::Feedback(signals)],
        });
    }
}

/// One action the agent has asked to take, paired with the originating tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub tool: String,
    pub call_id: String,
    pub args: serde_json::Value,
}
