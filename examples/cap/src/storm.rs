//! Tool-call storm guard — one of the cost/reliability lessons from Reasonix's
//! "the accounting is the moat" design: models (especially fast ones) sometimes
//! fall into a *call storm* — invoking the same tool with identical arguments
//! over and over (call → fail → retry → same call). Each repeat burns tokens
//! and makes no progress.
//!
//! This `Hook` watches `PreToolUse`, remembers the last few `(tool, args)`
//! fingerprints, and when it sees an exact repeat it **breaks the loop**:
//! returns `HookOutcome::Deny` with a reflection nudging the model to change
//! approach, and prints a visible "struggling" marker (the failure signal the
//! Reasonix TUI surfaces before escalating).

use harness_core::{Event, Hook, HookOutcome, World};
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

const WINDOW: usize = 12;

pub struct StormGuard {
    recent: Mutex<VecDeque<u64>>,
}

impl StormGuard {
    pub fn new() -> Self {
        Self {
            recent: Mutex::new(VecDeque::with_capacity(WINDOW)),
        }
    }
}

impl Default for StormGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic fingerprint of a tool call: name + canonical args JSON.
fn fingerprint(tool: &str, args: &serde_json::Value) -> u64 {
    let mut h = DefaultHasher::new();
    tool.hash(&mut h);
    // serde_json's Map keeps keys sorted by default → stable string per call.
    args.to_string().hash(&mut h);
    h.finish()
}

impl Hook for StormGuard {
    fn name(&self) -> &str {
        "storm-guard"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        let Event::PreToolUse { action } = ev else {
            return HookOutcome::Allow;
        };
        let fp = fingerprint(&action.tool, &action.args);
        let mut recent = self.recent.lock().unwrap();
        let repeats = recent.iter().filter(|&&h| h == fp).count();
        recent.push_back(fp);
        if recent.len() > WINDOW {
            recent.pop_front();
        }
        if repeats >= 1 {
            // Second identical call in the window → a storm. Break it.
            eprintln!(
                "\n  \x1b[33m⚠ storm: repeated {} with identical args — nudging a different approach\x1b[0m",
                action.tool
            );
            HookOutcome::Deny {
                reason: format!(
                    "You already called `{}` with these exact arguments and it did not \
                     resolve the task. Do NOT repeat it. Re-read the current state (e.g. \
                     hash_read) and try a materially different approach, or explain what's \
                     blocking you.",
                    action.tool
                ),
            }
        } else {
            HookOutcome::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Action;
    use serde_json::json;

    fn ev(tool: &str, args: serde_json::Value) -> Action {
        Action {
            tool: tool.into(),
            call_id: "c".into(),
            args,
        }
    }

    #[test]
    fn first_call_allowed_repeat_denied() {
        let g = StormGuard::new();
        let mut w = harness_context::default_world(".");
        let a = ev(
            "hash_edit",
            json!({"path": "x.rs", "edits": [{"op": "replace", "anchor": "dead"}]}),
        );

        // First identical call: allowed.
        let r1 = g.fire(&Event::PreToolUse { action: &a }, &mut w);
        assert!(matches!(r1, HookOutcome::Allow));
        // Exact repeat: denied (storm broken).
        let r2 = g.fire(&Event::PreToolUse { action: &a }, &mut w);
        assert!(matches!(r2, HookOutcome::Deny { .. }));

        // A different call is fine.
        let b = ev("hash_read", json!({"path": "x.rs"}));
        let r3 = g.fire(&Event::PreToolUse { action: &b }, &mut w);
        assert!(matches!(r3, HookOutcome::Allow));
    }
}
