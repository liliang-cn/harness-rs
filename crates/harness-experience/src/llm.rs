//! One-shot model calls, and getting JSON back out of a model that was asked
//! for JSON.
//!
//! Both halves of the learning loop spend **exactly one** model call, off the
//! critical path of the run they're learning from. Neither builds an `AgentLoop`
//! — there are no tools to call and nothing to iterate on, so a bare
//! `Model::complete` with a single user turn is the whole interaction. The
//! shape mirrors `harness_compactor::ModelBackedCompactor::model_summarise`,
//! which does the same thing for summarisation.

use harness_core::{Block, Context, Model, Policy, ResponseFormat, Task, Turn, TurnRole};
use serde_json::Value;
use std::sync::Arc;

/// Send `prompt` as a single user turn and return the model's text.
///
/// `ResponseFormat::JsonObject` is set because every caller here wants JSON;
/// adapters that support native JSON mode will constrain the decode, and the
/// ones that don't simply ignore it — which is why [`extract_json`] still has to
/// be forgiving.
pub(crate) async fn one_shot(
    model: &Arc<dyn Model>,
    prompt: String,
    max_output_tokens: u32,
) -> Result<String, String> {
    let mut ctx = Context::new(Task {
        description: prompt.clone(),
        source: None,
        deadline: None,
    });
    ctx.policy = Policy {
        max_iters: 1,
        max_input_tokens: 100_000,
        max_output_tokens,
        self_correct_rounds: 0,
    };
    ctx.response_format = ResponseFormat::JsonObject;
    ctx.history.push(Turn {
        role: TurnRole::User,
        blocks: vec![Block::Text(prompt)],
    });
    let out = model.complete(&ctx).await.map_err(|e| e.to_string())?;
    Ok(out.text.unwrap_or_default())
}

/// Pull a JSON object out of a model reply.
///
/// Three escalating attempts, because "reply with only JSON" is a request, not a
/// guarantee: (1) the whole reply, (2) the contents of a ``` fence, (3) the span
/// from the first `{` to the last `}`. Step 3 is what rescues the very common
/// "Sure! Here's the skill:\n{...}\nLet me know if…" reply.
pub(crate) fn extract_json(raw: &str) -> Option<Value> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        return Some(v);
    }
    let unfenced = strip_code_fence(raw);
    if let Ok(v) = serde_json::from_str::<Value>(unfenced.trim()) {
        return Some(v);
    }
    let start = unfenced.find('{')?;
    let end = unfenced.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&unfenced[start..=end]).ok()
}

/// Strip one ```/```json fence if the text is wrapped in one.
fn strip_code_fence(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else {
        return s;
    };
    // Drop the optional language tag on the opening fence line.
    let rest = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => return s,
    };
    match rest.rfind("```") {
        Some(i) => &rest[..i],
        None => rest,
    }
}

/// Read a required non-empty string field.
pub(crate) fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Read a string array, tolerating the common "one newline-separated string"
/// degradation. Empty entries are dropped.
pub(crate) fn str_list(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.trim().to_string()),
                // A model occasionally answers `[{"step": "..."}]`.
                Value::Object(o) => o
                    .values()
                    .find_map(Value::as_str)
                    .map(|s| s.trim().to_string()),
                other => Some(other.to_string()),
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .lines()
            .map(|l| l.trim().trim_start_matches(['-', '*', '•']).trim())
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bare_json() {
        let v = extract_json(r#"{"name":"x"}"#).unwrap();
        assert_eq!(v["name"], "x");
    }

    #[test]
    fn extracts_fenced_json() {
        let v = extract_json("```json\n{\"name\":\"x\"}\n```").unwrap();
        assert_eq!(v["name"], "x");
    }

    #[test]
    fn extracts_json_buried_in_prose() {
        let v = extract_json("Sure! Here you go:\n{\"name\": \"x\"}\nHope that helps.").unwrap();
        assert_eq!(v["name"], "x");
    }

    #[test]
    fn rejects_non_json() {
        assert!(extract_json("I'm sorry, I can't help with that.").is_none());
        assert!(extract_json("").is_none());
    }

    #[test]
    fn str_list_handles_arrays_and_newline_strings() {
        let v = serde_json::json!({"steps": ["a", "b"], "alt": "- x\n- y\n"});
        assert_eq!(str_list(&v, "steps"), vec!["a", "b"]);
        assert_eq!(str_list(&v, "alt"), vec!["x", "y"]);
        assert!(str_list(&v, "missing").is_empty());
    }

    #[test]
    fn str_field_rejects_blank() {
        let v = serde_json::json!({"a": "  ", "b": "ok"});
        assert_eq!(str_field(&v, "a"), None);
        assert_eq!(str_field(&v, "b").as_deref(), Some("ok"));
    }
}
