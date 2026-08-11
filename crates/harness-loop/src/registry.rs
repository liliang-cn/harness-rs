//! A tiny name-keyed tool registry used by `AgentLoop`.

use harness_core::{Action, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, t: Arc<dyn Tool>) {
        self.tools.insert(t.name().to_string(), t);
    }

    /// Tool schemas in a **stable, name-sorted order**. Deterministic ordering
    /// keeps the request's `tools` block byte-identical across turns, which is
    /// what lets a provider's prefix cache (e.g. DeepSeek) hit — a `HashMap`'s
    /// arbitrary iteration order would silently break it.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let mut v: Vec<ToolSchema> = self.tools.values().map(|t| t.schema().clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub async fn dispatch(
        &self,
        action: &Action,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(&action.tool)
            .ok_or_else(|| ToolError::NotFound {
                name: action.tool.clone(),
                hint: self.not_found_hint(&action.tool),
            })?
            .clone();
        tool.invoke(action.args.clone(), world).await
    }

    /// The correction appended to a "tool not found" error.
    ///
    /// A bare "tool `read_files` not found" is the model's only clue, so the next
    /// turn is another guess — the failure costs a whole round trip, often more
    /// than one, and a small model can spend the whole budget circling a name it
    /// nearly had. Naming the nearest tool, then listing the rest, turns that
    /// into a single-turn correction.
    fn not_found_hint(&self, wanted: &str) -> String {
        let mut names: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        if names.is_empty() {
            return " (no tools are registered on this agent)".into();
        }
        let closest = names
            .iter()
            .map(|n| (edit_distance(wanted, n), *n))
            // Only offer a correction that is plausibly a typo of what was asked;
            // suggesting `grep` for `book_flight` is worse than suggesting nothing.
            .filter(|(d, n)| *d * 3 <= n.len().max(wanted.len()))
            .min_by_key(|(d, _)| *d)
            .map(|(_, n)| n);
        let available = names.join(", ");
        match closest {
            Some(c) => format!(" — did you mean `{c}`? available tools: {available}"),
            None => format!(" — available tools: {available}"),
        }
    }

    /// The risk class of a tool by name (used to decide parallel-safe dispatch).
    pub fn risk(&self, name: &str) -> Option<ToolRisk> {
        self.tools.get(name).map(|t| t.risk())
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Levenshtein distance, iterative with a single row. Small alphabets, short
/// strings — a tool registry is a handful of names, so the naive version is
/// well inside its budget.
fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut cur = vec![0usize; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::{ToolRisk, ToolSchema};

    struct Named(&'static str);
    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn schema(&self) -> &ToolSchema {
            static S: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
            S.get_or_init(|| ToolSchema {
                name: "x".into(),
                description: String::new(),
                input: serde_json::json!({"type": "object"}),
            })
        }
        fn risk(&self) -> ToolRisk {
            ToolRisk::ReadOnly
        }
        async fn invoke(
            &self,
            _a: serde_json::Value,
            _w: &mut World,
        ) -> Result<ToolResult, ToolError> {
            unreachable!("never dispatched in these tests")
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for n in ["read_file", "write_file", "list_dir", "grep"] {
            r.insert(Arc::new(Named(n)));
        }
        r
    }

    /// The commonest miss is a near-miss — a plural, a tense, a separator. The
    /// model gets one line back and has to decide the next turn from it.
    #[test]
    fn a_near_miss_is_corrected_by_name() {
        let hint = registry().not_found_hint("read_files");
        assert!(hint.contains("did you mean `read_file`"), "{hint}");
        assert!(hint.contains("write_file"), "and lists the rest: {hint}");
    }

    /// A name nothing like the registry gets the list, not a misleading guess:
    /// pointing at `grep` for `book_flight` sends the model somewhere wrong with
    /// confidence, which costs more than saying nothing.
    #[test]
    fn an_unrelated_name_gets_no_guess() {
        let hint = registry().not_found_hint("book_flight");
        assert!(!hint.contains("did you mean"), "{hint}");
        assert!(hint.contains("available tools: grep, list_dir"), "{hint}");
    }

    #[test]
    fn an_empty_registry_says_so() {
        let hint = ToolRegistry::new().not_found_hint("anything");
        assert!(hint.contains("no tools are registered"), "{hint}");
    }

    /// The hint is part of the error the loop feeds back, not a side channel.
    #[tokio::test]
    async fn dispatch_surfaces_the_hint_in_the_error() {
        let ws = std::env::temp_dir().join(format!("registry-nf-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut world = harness_context::default_world(&ws);
        let err = registry()
            .dispatch(
                &Action {
                    tool: "read_fil".into(),
                    call_id: "1".into(),
                    args: serde_json::json!({}),
                },
                &mut world,
            )
            .await
            .unwrap_err();
        let _ = std::fs::remove_dir_all(&ws);
        let msg = err.to_string();
        assert!(msg.contains("read_fil"), "{msg}");
        assert!(msg.contains("did you mean `read_file`"), "{msg}");
    }
}
