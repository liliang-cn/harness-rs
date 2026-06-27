//! `LoopSpec` — the declarative description of a loop.
//!
//! A spec is pure data: it says *what* the loop is, at what maturity, on
//! what cadence, under what budget, and what the maker and checker should
//! each try to do. It carries no models, tools, or `Arc`s — those are bound
//! when you build a [`crate::LoopEngine`] from the spec. Keeping the spec
//! inert means it can be cloned, serialized, diffed, and unit-tested on its
//! own, and that the production [`crate::patterns`] are just constructors
//! returning one of these.

use crate::budget::TokenBudget;
use crate::level::LoopLevel;
use serde::{Deserialize, Serialize};

/// Declarative definition of a single loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSpec {
    /// Stable identifier — used in logs, memory keys, and scheduler jobs.
    pub name: String,

    /// **Intent.** One sentence stating what this loop is *supposed* to do.
    ///
    /// This is the antidote to *intent debt* — the drift between what a loop
    /// was meant to do and what it actually does. Writing it down, surfacing
    /// it in every report, and reviewing it keeps the gap visible. It is
    /// also injected into the maker's task so the agent shares the framing.
    pub intent: String,

    /// Maturity level — governs write-capability and gate policy.
    pub level: LoopLevel,

    /// Cadence string parsed by `harness_daemon::Schedule`
    /// (e.g. `"every 15m"`, `"daily 08:00"`, `"weekly mon 09:30"`).
    pub cadence: String,

    /// Spend ceiling enforced per round.
    pub budget: TokenBudget,

    /// What the maker sub-agent is asked to do this round (triage +
    /// implement, or — at L1 — just investigate and report).
    pub maker_prompt: String,

    /// What the checker sub-agent is asked to verify (run tests, check
    /// gates, look for regressions). The maker/checker split is loop
    /// engineering's verification discipline made structural.
    pub checker_prompt: String,

    /// The `kind` of the action this loop proposes when its work verifies,
    /// e.g. `"open-pr"`, `"commit"`, `"comment"`, `"report"`. The gate
    /// matches its allowlist against this.
    pub action_kind: String,
}

impl LoopSpec {
    /// Minimal constructor; fill the rest with the builder methods.
    pub fn new(name: impl Into<String>, intent: impl Into<String>, level: LoopLevel) -> Self {
        Self {
            name: name.into(),
            intent: intent.into(),
            level,
            cadence: "daily 09:00".into(),
            budget: TokenBudget::default(),
            maker_prompt: String::new(),
            checker_prompt: String::new(),
            action_kind: "report".into(),
        }
    }

    pub fn with_cadence(mut self, c: impl Into<String>) -> Self {
        self.cadence = c.into();
        self
    }
    pub fn with_budget(mut self, b: TokenBudget) -> Self {
        self.budget = b;
        self
    }
    pub fn with_maker_prompt(mut self, p: impl Into<String>) -> Self {
        self.maker_prompt = p.into();
        self
    }
    pub fn with_checker_prompt(mut self, p: impl Into<String>) -> Self {
        self.checker_prompt = p.into();
        self
    }
    pub fn with_action_kind(mut self, k: impl Into<String>) -> Self {
        self.action_kind = k.into();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_roundtrips_through_json() {
        let spec = LoopSpec::new(
            "issue-triage",
            "label and route new issues",
            LoopLevel::L1Report,
        )
        .with_cadence("every 2h")
        .with_action_kind("comment");
        let json = serde_json::to_string(&spec).unwrap();
        let back: LoopSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "issue-triage");
        assert_eq!(back.level, LoopLevel::L1Report);
        assert_eq!(back.cadence, "every 2h");
        assert_eq!(back.action_kind, "comment");
    }
}
