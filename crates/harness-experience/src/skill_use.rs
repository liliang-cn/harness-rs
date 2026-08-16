//! `SkillUseTrace` — record which skills a run followed.
//!
//! [`crate::SkillReviser`] needs to know that a bad run was following skill X.
//! Nothing in the framework knows that today: skills reach the model as prompt
//! text (a catalogue, then a body), and there is no `SkillActivated` lifecycle
//! event to hook — unlike tools, which [`crate::ToolTrace`] can capture from
//! `PreToolUse`.
//!
//! So this is deliberately thin: a shared buffer with the same
//! `snapshot`/`drain` shape as [`crate::ToolTrace`], filled either directly by
//! the host (which knows what it injected) or via `Event::Custom` for hosts that
//! would rather emit a signal than thread a handle around. It is *not* an
//! inference layer — guessing which skill an agent followed by scanning its
//! output would mislabel episodes, and a mislabelled episode is worse than an
//! unlabelled one: it revises the wrong skill.
//!
//! ```ignore
//! let skills = SkillUseTrace::new();
//! skills.note("deploy-runbook");                 // host injected this skill
//! let loop_ = AgentLoop::new(model).with_hook(skills.hook());  // or emit events
//! // …
//! recorder.record_episode(
//!     Episode::new(&task.description, outcome)
//!         .with_success(ok)
//!         .with_skills(skills.drain()),
//! ).await;
//! ```

use harness_core::{Event, Hook, HookOutcome, World};
use std::sync::{Arc, Mutex};

/// The `Event::Custom` name [`SkillUseTrace`]'s hook listens for.
pub const SKILL_ACTIVATED_EVENT: &str = "skill.activated";

/// Shared, cloneable buffer of skill names, deduplicated, in first-use order.
///
/// Deduplicated because — unlike a tool trace, where repetition is signal about
/// the *approach* — a skill re-read three times is still one skill, and
/// `Episode::skills` is a set-membership question ("was X in play?").
#[derive(Clone, Default)]
pub struct SkillUseTrace {
    skills: Arc<Mutex<Vec<String>>>,
}

impl SkillUseTrace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the run is following `name`. Idempotent.
    pub fn note(&self, name: impl Into<String>) {
        let name = name.into();
        if name.trim().is_empty() {
            return;
        }
        if let Ok(mut g) = self.skills.lock()
            && !g.iter().any(|s| s == &name)
        {
            g.push(name);
        }
    }

    /// The skills recorded so far.
    pub fn snapshot(&self) -> Vec<String> {
        self.skills.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Take the recorded skills and reset the buffer (call after a run).
    pub fn drain(&self) -> Vec<String> {
        self.skills
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    /// A `Hook` that fills this buffer from
    /// `Event::Custom { name: "skill.activated", data: {"skill": "<name>"} }`.
    ///
    /// `Event::Custom` is the framework's escape hatch for signals it doesn't
    /// model natively, which is exactly what skill activation currently is.
    /// Emitting one keeps the host's skill-injection code from having to hold a
    /// `SkillUseTrace` handle.
    pub fn hook(&self) -> Arc<dyn Hook> {
        Arc::new(SkillUseHook {
            skills: self.skills.clone(),
        })
    }
}

struct SkillUseHook {
    skills: Arc<Mutex<Vec<String>>>,
}

impl Hook for SkillUseHook {
    fn name(&self) -> &str {
        "experience-skill-use-trace"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::Custom { name, .. } if *name == SKILL_ACTIVATED_EVENT)
    }
    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::Custom { data, .. } = ev {
            // Accept `{"skill": "x"}`, `{"skills": ["x", "y"]}`, or a bare
            // string — hosts emit whichever is convenient, and dropping the
            // signal over its shape would lose the episode's only skill label.
            let names: Vec<String> = match data {
                serde_json::Value::String(s) => vec![s.clone()],
                v => {
                    let mut out = Vec::new();
                    if let Some(s) = v.get("skill").and_then(|s| s.as_str()) {
                        out.push(s.to_string());
                    }
                    if let Some(arr) = v.get("skills").and_then(|s| s.as_array()) {
                        out.extend(arr.iter().filter_map(|i| i.as_str()).map(str::to_string));
                    }
                    out
                }
            };
            for n in names {
                self.note_locked(&n);
            }
        }
        HookOutcome::Allow
    }
}

impl SkillUseHook {
    fn note_locked(&self, name: &str) {
        if name.trim().is_empty() {
            return;
        }
        if let Ok(mut g) = self.skills.lock()
            && !g.iter().any(|s| s == name)
        {
            g.push(name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn note_dedupes_and_drain_resets() {
        let t = SkillUseTrace::new();
        t.note("deploy-runbook");
        t.note("deploy-runbook");
        t.note("changelog-writing");
        t.note("   ");
        assert_eq!(t.snapshot(), vec!["deploy-runbook", "changelog-writing"]);
        assert_eq!(t.drain().len(), 2);
        assert!(t.snapshot().is_empty(), "drain resets");
    }

    #[test]
    fn hook_captures_every_payload_shape() {
        let t = SkillUseTrace::new();
        let hook = t.hook();
        let mut world = harness_context::default_world(std::env::temp_dir());
        for data in [
            json!({"skill": "a"}),
            json!({"skills": ["b", "c"]}),
            json!("d"),
            json!({"skill": "a"}), // duplicate
        ] {
            hook.fire(
                &Event::Custom {
                    name: SKILL_ACTIVATED_EVENT,
                    data: &data,
                },
                &mut world,
            );
        }
        assert_eq!(t.snapshot(), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn hook_ignores_other_custom_events() {
        let t = SkillUseTrace::new();
        let hook = t.hook();
        let data = json!({"skill": "a"});
        assert!(!hook.matches(&Event::Custom {
            name: "something.else",
            data: &data,
        }));
        assert!(hook.matches(&Event::Custom {
            name: SKILL_ACTIVATED_EVENT,
            data: &data,
        }));
    }
}
