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
            _ => "unknown",
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
