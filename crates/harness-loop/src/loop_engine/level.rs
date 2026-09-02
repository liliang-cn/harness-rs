//! Loop maturity levels and the human-gate abstraction.
//!
//! Loop engineering's central safety idea: a loop earns autonomy in
//! stages. You start **report-only**, graduate to **assisted** (the loop
//! proposes, a human approves), and only the narrowest, well-fenced loops
//! ever run **unattended**. Each level changes two things: whether the
//! maker sub-agent may write at all, and how the gate decides what to do
//! with a verified proposal.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// How much autonomy a loop is trusted with.
///
/// The level is the single knob that ties together write-capability and
/// gate policy — it is *not* the same as a tool permission mode (that is
/// `harness-permissions`, which the engine derives from the level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoopLevel {
    /// **L1 — Report-only.** Discovery and visibility, no automated fixes.
    /// The maker runs read-only; every proposal is escalated to a human as
    /// a report. The safest place to start any loop.
    L1Report,
    /// **L2 — Assisted.** The maker may write inside an isolated sandbox,
    /// but a human gates every change before it lands. The default for a
    /// loop you are actively building trust in.
    L2Assisted,
    /// **L3 — Unattended.** Runs within strict guardrails: only actions on
    /// the gate's allowlist commit automatically; anything else still
    /// escalates. Reserve for narrow, well-understood loops.
    L3Unattended,
}

impl LoopLevel {
    /// Whether the maker sub-agent is permitted to mutate the workspace at
    /// this level. L1 is strictly read-only.
    pub fn maker_may_write(self) -> bool {
        !matches!(self, LoopLevel::L1Report)
    }

    /// Short stable label for logs, reports, and memory entries.
    pub fn label(self) -> &'static str {
        match self {
            LoopLevel::L1Report => "L1-report",
            LoopLevel::L2Assisted => "L2-assisted",
            LoopLevel::L3Unattended => "L3-unattended",
        }
    }
}

/// A change the maker produced and the checker verified, presented to the
/// gate for a proceed-or-escalate decision.
#[derive(Debug, Clone)]
pub struct ProposedAction {
    /// Stable kind used for allowlisting, e.g. `"commit"`, `"open-pr"`,
    /// `"apply-patch"`, `"comment"`. Free-form; the gate matches on it.
    pub kind: String,
    /// Human-readable summary of what the loop wants to do.
    pub summary: String,
    /// Whether the checker reported the work as clean (tests/gates passed).
    /// A gate may auto-proceed only on verified work.
    pub verified: bool,
}

impl ProposedAction {
    pub fn new(kind: impl Into<String>, summary: impl Into<String>, verified: bool) -> Self {
        Self {
            kind: kind.into(),
            summary: summary.into(),
            verified,
        }
    }
}

/// The gate's verdict for one proposed action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Safe / allowlisted / verified — let the loop carry the action out.
    AutoProceed,
    /// Risky or ambiguous — hand it to a human with context. The loop
    /// records the escalation and recurses on the next tick.
    Escalate { reason: String },
}

impl GateDecision {
    pub fn escalate(reason: impl Into<String>) -> Self {
        GateDecision::Escalate {
            reason: reason.into(),
        }
    }
    pub fn is_auto(&self) -> bool {
        matches!(self, GateDecision::AutoProceed)
    }
}

/// Decides what happens to a verified proposal. Implementations encode the
/// human-gate policy; the engine consults this once per round.
pub trait HumanGate: Send + Sync {
    fn decide(&self, level: LoopLevel, action: &ProposedAction) -> GateDecision;
}

/// Never auto-proceeds — every proposal is escalated. This is the correct
/// gate for L1 loops (and a safe default for anything you're unsure about).
pub struct AlwaysEscalate;

impl HumanGate for AlwaysEscalate {
    fn decide(&self, _level: LoopLevel, _action: &ProposedAction) -> GateDecision {
        GateDecision::escalate("report-only / human review required")
    }
}

/// Auto-proceeds only for verified actions whose `kind` is on the
/// allowlist, and only at L3. Everything else escalates. This is the
/// workhorse gate for unattended loops with a narrow blast radius.
pub struct AllowlistGate {
    allow_kinds: Vec<String>,
}

impl AllowlistGate {
    pub fn new<I, S>(kinds: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allow_kinds: kinds.into_iter().map(Into::into).collect(),
        }
    }
}

impl HumanGate for AllowlistGate {
    fn decide(&self, level: LoopLevel, action: &ProposedAction) -> GateDecision {
        if level != LoopLevel::L3Unattended {
            return GateDecision::escalate("allowlist gate only auto-proceeds at L3");
        }
        if !action.verified {
            return GateDecision::escalate("checker did not verify the work");
        }
        if self.allow_kinds.iter().any(|k| k == &action.kind) {
            GateDecision::AutoProceed
        } else {
            GateDecision::escalate(format!("action kind `{}` is not allowlisted", action.kind))
        }
    }
}

/// Wraps an arbitrary closure as a gate — for custom policies (budget-aware,
/// time-of-day, denylist, MCP-scope checks, …).
pub struct CallbackGate<F>(pub F)
where
    F: Fn(LoopLevel, &ProposedAction) -> GateDecision + Send + Sync;

impl<F> HumanGate for CallbackGate<F>
where
    F: Fn(LoopLevel, &ProposedAction) -> GateDecision + Send + Sync,
{
    fn decide(&self, level: LoopLevel, action: &ProposedAction) -> GateDecision {
        (self.0)(level, action)
    }
}

/// The gate a level implies when the caller doesn't specify one.
///
/// L1 and L2 both default to [`AlwaysEscalate`] — L1 because it only ever
/// reports, L2 because a human must gate every change. L3 has no safe
/// default (an unattended loop needs an explicit allowlist), so it also
/// defaults to `AlwaysEscalate` until the caller supplies an
/// [`AllowlistGate`] via `LoopEngine::with_gate`.
pub fn default_gate_for(_level: LoopLevel) -> Arc<dyn HumanGate> {
    Arc::new(AlwaysEscalate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l1_is_read_only() {
        assert!(!LoopLevel::L1Report.maker_may_write());
        assert!(LoopLevel::L2Assisted.maker_may_write());
        assert!(LoopLevel::L3Unattended.maker_may_write());
    }

    #[test]
    fn always_escalate_never_proceeds() {
        let g = AlwaysEscalate;
        let a = ProposedAction::new("commit", "x", true);
        assert!(!g.decide(LoopLevel::L3Unattended, &a).is_auto());
    }

    #[test]
    fn allowlist_gate_only_auto_at_l3_for_verified_allowlisted() {
        let g = AllowlistGate::new(["comment", "commit"]);
        // L3 + verified + allowlisted -> auto
        assert!(
            g.decide(
                LoopLevel::L3Unattended,
                &ProposedAction::new("commit", "s", true)
            )
            .is_auto()
        );
        // not allowlisted -> escalate
        assert!(
            !g.decide(
                LoopLevel::L3Unattended,
                &ProposedAction::new("force-push", "s", true)
            )
            .is_auto()
        );
        // not verified -> escalate
        assert!(
            !g.decide(
                LoopLevel::L3Unattended,
                &ProposedAction::new("commit", "s", false)
            )
            .is_auto()
        );
        // wrong level -> escalate
        assert!(
            !g.decide(
                LoopLevel::L2Assisted,
                &ProposedAction::new("commit", "s", true)
            )
            .is_auto()
        );
    }

    #[test]
    fn callback_gate_runs_closure() {
        let g = CallbackGate(|_lvl, a: &ProposedAction| {
            if a.kind == "ok" {
                GateDecision::AutoProceed
            } else {
                GateDecision::escalate("nope")
            }
        });
        assert!(
            g.decide(
                LoopLevel::L3Unattended,
                &ProposedAction::new("ok", "", true)
            )
            .is_auto()
        );
    }
}
