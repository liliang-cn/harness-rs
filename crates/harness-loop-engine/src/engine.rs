//! `LoopEngine` — the runner that turns a [`LoopSpec`] into recursive,
//! verified, budgeted, gated agent work.
//!
//! One `run_once` is one trip around the anatomical loop:
//!
//! ```text
//! recall state (memory)  ->  isolated sandbox  ->  maker sub-agent
//!   ->  checker sub-agent (tests + gates)  ->  human gate  ->  record state
//! ```
//!
//! The maker/checker split is the verification discipline made structural:
//! one sub-agent proposes, a second, independent one tries to confirm the
//! work is clean. The gate then decides — within the loop's maturity level —
//! whether to proceed automatically or escalate to a human. Every round is
//! bounded by a [`TokenBudget`]; every round's outcome is written back to
//! memory as the durable spine that lets the next round pick up where this
//! one left off.

use crate::budget::{BudgetLimit, BudgetState};
use crate::level::{GateDecision, HumanGate, LoopLevel, ProposedAction};
use crate::spec::LoopSpec;
use harness_core::{Memory, MemoryEntry, Model, SubagentStatus, Task, Tool};
use harness_loop::{Subagent, SubagentReport, SubagentSpec};
use harness_sandbox::{NullSandbox, Sandbox};
use std::path::PathBuf;
use std::sync::Arc;

/// What one round of a loop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// L1 (or any escalate-by-design round): the loop investigated and the
    /// finding is delivered as a report. No change was applied.
    Reported,
    /// The gate auto-approved a verified proposal — the loop is cleared to
    /// carry out its action (commit / PR / comment / …).
    Proceeded,
    /// Handed to a human with context. The loop will recurse next tick.
    Escalated { reason: String },
    /// A spend ceiling was crossed mid-round; the loop stopped early.
    BudgetExhausted { limit: BudgetLimit },
    /// The sandbox, maker, or checker errored. Best-effort: the scheduler
    /// keeps ticking; this round simply produced nothing actionable.
    Failed { error: String },
}

/// The full record of a round — the maker/checker reports, token spend, the
/// gate decision, and the outcome. Suitable for delivery to a channel and
/// for writing to memory.
#[derive(Debug, Clone)]
pub struct RoundReport {
    pub loop_name: String,
    pub intent: String,
    pub level: LoopLevel,
    pub maker: Option<SubagentReport>,
    pub checker: Option<SubagentReport>,
    pub decision: Option<GateDecision>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub outcome: RoundOutcome,
}

impl RoundReport {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Whether this round produced something worth delivering to a human.
    /// A clean auto-proceed at L3 is intentionally quiet.
    pub fn should_deliver(&self) -> bool {
        !matches!(self.outcome, RoundOutcome::Proceeded)
    }

    /// A compact, human-readable summary for channels and memory.
    pub fn render(&self) -> String {
        let mut s = format!(
            "[{}] loop `{}` ({})\nintent: {}\n",
            self.level.label(),
            self.loop_name,
            outcome_label(&self.outcome),
            self.intent
        );
        if let Some(m) = &self.maker {
            s.push_str(&format!("maker: {:?} in {} iters\n", m.status, m.iters));
            if let Some(t) = &m.text {
                s.push_str(&format!("{}\n", t.trim()));
            }
        }
        if let Some(c) = &self.checker {
            s.push_str(&format!("checker: {:?} in {} iters\n", c.status, c.iters));
        }
        if let RoundOutcome::Escalated { reason } = &self.outcome {
            s.push_str(&format!("escalation: {reason}\n"));
        }
        s.push_str(&format!(
            "tokens: {} in / {} out\n",
            self.input_tokens, self.output_tokens
        ));
        s
    }
}

fn outcome_label(o: &RoundOutcome) -> &'static str {
    match o {
        RoundOutcome::Reported => "reported",
        RoundOutcome::Proceeded => "proceeded",
        RoundOutcome::Escalated { .. } => "escalated",
        RoundOutcome::BudgetExhausted { .. } => "budget-exhausted",
        RoundOutcome::Failed { .. } => "failed",
    }
}

/// Binds a [`LoopSpec`] to the live pieces it needs to run: a model, the
/// maker/checker tool sets, an isolation sandbox, a gate, and (optionally)
/// memory for the state spine.
pub struct LoopEngine {
    spec: LoopSpec,
    model: Arc<dyn Model>,
    maker_tools: Vec<Arc<dyn Tool>>,
    checker_tools: Vec<Arc<dyn Tool>>,
    sandbox: Arc<dyn Sandbox>,
    gate: Arc<dyn HumanGate>,
    memory: Option<Arc<dyn Memory>>,
}

impl LoopEngine {
    /// Construct an engine. By default the maker and checker run with no
    /// tools, in a [`NullSandbox`] rooted at the current directory, with the
    /// gate the spec's level implies (`AlwaysEscalate` for L1/L2). Override
    /// any of these with the builder methods.
    pub fn new(spec: LoopSpec, model: Arc<dyn Model>) -> Self {
        let gate = crate::level::default_gate_for(spec.level);
        Self {
            spec,
            model,
            maker_tools: Vec::new(),
            checker_tools: Vec::new(),
            sandbox: Arc::new(NullSandbox::new(PathBuf::from("."))),
            gate,
            memory: None,
        }
    }

    pub fn with_maker_tool(mut self, t: Arc<dyn Tool>) -> Self {
        self.maker_tools.push(t);
        self
    }
    pub fn with_checker_tool(mut self, t: Arc<dyn Tool>) -> Self {
        self.checker_tools.push(t);
        self
    }
    pub fn with_sandbox(mut self, s: Arc<dyn Sandbox>) -> Self {
        self.sandbox = s;
        self
    }
    pub fn with_gate(mut self, g: Arc<dyn HumanGate>) -> Self {
        self.gate = g;
        self
    }
    pub fn with_memory(mut self, m: Arc<dyn Memory>) -> Self {
        self.memory = Some(m);
        self
    }

    pub fn spec(&self) -> &LoopSpec {
        &self.spec
    }

    /// Run exactly one round of the loop. Never panics and never returns an
    /// `Err`: sandbox/maker/checker failures are folded into
    /// [`RoundOutcome::Failed`] so a scheduler can keep ticking. The result
    /// is also recorded to memory when memory is configured.
    pub async fn run_once(&self) -> RoundReport {
        let report = self.run_round().await;
        self.record(&report).await;
        report
    }

    async fn run_round(&self) -> RoundReport {
        let mut budget = BudgetState::new(self.spec.budget);
        let level = self.spec.level;

        // --- Triage: recall prior state from memory. ---
        let prior = self.recall_state().await;

        // --- Isolated sandbox for this round. ---
        let mut handle = match self.sandbox.spawn().await {
            Ok(h) => h,
            Err(e) => {
                return self.failed(format!("sandbox spawn failed: {e}"), &budget, None, None);
            }
        };

        // --- Maker sub-agent. ---
        let maker_desc = self.maker_task_description(&prior);
        let maker = SubagentSpec::new(
            format!("{}:maker", self.spec.name),
            Task {
                description: maker_desc,
                source: None,
                deadline: None,
            },
        )
        .with_max_iters(budget.max_iters());
        let maker = with_tools(maker, &self.maker_tools);
        let maker_report = match Subagent::new(dyn_model(&self.model), maker)
            .run(&mut handle.world)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return self.failed(format!("maker failed: {e}"), &budget, None, None);
            }
        };
        budget.add(&maker_report.usage);
        if let Some(limit) = budget.exceeded() {
            return self.budget_exhausted(limit, &budget, Some(maker_report), None);
        }

        // --- Checker sub-agent (verification). ---
        let checker_desc = self.checker_task_description(&maker_report);
        let checker = SubagentSpec::new(
            format!("{}:checker", self.spec.name),
            Task {
                description: checker_desc,
                source: None,
                deadline: None,
            },
        )
        .with_max_iters(budget.max_iters());
        let checker = with_tools(checker, &self.checker_tools);
        let checker_report = match Subagent::new(dyn_model(&self.model), checker)
            .run(&mut handle.world)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return self.failed(
                    format!("checker failed: {e}"),
                    &budget,
                    Some(maker_report),
                    None,
                );
            }
        };
        budget.add(&checker_report.usage);
        if let Some(limit) = budget.exceeded() {
            return self.budget_exhausted(limit, &budget, Some(maker_report), Some(checker_report));
        }

        // --- Gate: proceed or escalate. ---
        let verified = checker_report.status == SubagentStatus::Done;
        let summary = checker_report
            .text
            .clone()
            .or_else(|| maker_report.text.clone())
            .unwrap_or_else(|| self.spec.intent.clone());
        let proposed = ProposedAction::new(self.spec.action_kind.clone(), summary, verified);
        let decision = self.gate.decide(level, &proposed);

        let outcome = match (&decision, level) {
            // L1 never acts — it reports, regardless of the gate verdict.
            (_, LoopLevel::L1Report) => RoundOutcome::Reported,
            (GateDecision::AutoProceed, _) => RoundOutcome::Proceeded,
            (GateDecision::Escalate { reason }, _) => RoundOutcome::Escalated {
                reason: reason.clone(),
            },
        };

        RoundReport {
            loop_name: self.spec.name.clone(),
            intent: self.spec.intent.clone(),
            level,
            maker: Some(maker_report),
            checker: Some(checker_report),
            decision: Some(decision),
            input_tokens: budget.input_tokens,
            output_tokens: budget.output_tokens,
            outcome,
        }
    }

    fn maker_task_description(&self, prior: &Option<String>) -> String {
        let write_note = if self.spec.level.maker_may_write() {
            "You MAY modify files in this workspace to accomplish the task."
        } else {
            "READ-ONLY: do NOT modify any files. Investigate and report findings only."
        };
        let mut d = format!(
            "Loop intent: {}\nMaturity level: {}\n{}\n\nTask:\n{}",
            self.spec.intent,
            self.spec.level.label(),
            write_note,
            self.spec.maker_prompt,
        );
        if let Some(p) = prior {
            d.push_str(&format!("\n\nState from previous rounds:\n{p}"));
        }
        d
    }

    fn checker_task_description(&self, maker: &SubagentReport) -> String {
        format!(
            "You are the checker (verifier) for loop `{}`.\nLoop intent: {}\n\n\
             Verify the work below. Run any available tests and gates, look for \
             regressions, and decide whether it is safe. Report DoneWithConcerns \
             if anything is questionable.\n\nMaker's report:\n{}\n\n\
             Verification task:\n{}",
            self.spec.name,
            self.spec.intent,
            maker.text.as_deref().unwrap_or("(maker produced no text)"),
            self.spec.checker_prompt,
        )
    }

    async fn recall_state(&self) -> Option<String> {
        let mem = self.memory.as_ref()?;
        match mem.recall(&self.spec.name, 5).await {
            Ok(hits) if !hits.is_empty() => Some(
                hits.iter()
                    .map(|e| format!("- {}", e.content))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(loop = %self.spec.name, error = %e, "loop-engine: recall failed");
                None
            }
        }
    }

    async fn record(&self, report: &RoundReport) {
        let Some(mem) = self.memory.as_ref() else {
            return;
        };
        let entry = MemoryEntry::new(format!(
            "{} — {}",
            outcome_label(&report.outcome),
            report
                .checker
                .as_ref()
                .and_then(|c| c.text.clone())
                .or_else(|| report.maker.as_ref().and_then(|m| m.text.clone()))
                .unwrap_or_else(|| report.intent.clone())
        ))
        .with_tags([self.spec.name.clone(), "loop-state".into()])
        .with_source(format!("loop:{}", self.spec.name));
        if let Err(e) = mem.write(entry).await {
            tracing::warn!(loop = %self.spec.name, error = %e, "loop-engine: state write failed");
        }
    }

    fn failed(
        &self,
        error: String,
        budget: &BudgetState,
        maker: Option<SubagentReport>,
        checker: Option<SubagentReport>,
    ) -> RoundReport {
        tracing::warn!(loop = %self.spec.name, %error, "loop-engine: round failed");
        RoundReport {
            loop_name: self.spec.name.clone(),
            intent: self.spec.intent.clone(),
            level: self.spec.level,
            maker,
            checker,
            decision: None,
            input_tokens: budget.input_tokens,
            output_tokens: budget.output_tokens,
            outcome: RoundOutcome::Failed { error },
        }
    }

    fn budget_exhausted(
        &self,
        limit: BudgetLimit,
        budget: &BudgetState,
        maker: Option<SubagentReport>,
        checker: Option<SubagentReport>,
    ) -> RoundReport {
        tracing::info!(loop = %self.spec.name, limit = limit.label(), "loop-engine: budget exhausted");
        RoundReport {
            loop_name: self.spec.name.clone(),
            intent: self.spec.intent.clone(),
            level: self.spec.level,
            maker,
            checker,
            decision: None,
            input_tokens: budget.input_tokens,
            output_tokens: budget.output_tokens,
            outcome: RoundOutcome::BudgetExhausted { limit },
        }
    }
}

fn dyn_model(m: &Arc<dyn Model>) -> harness_core::DynModel {
    harness_core::DynModel(m.clone())
}

fn with_tools(mut spec: SubagentSpec, tools: &[Arc<dyn Tool>]) -> SubagentSpec {
    for t in tools {
        spec = spec.with_tool(t.clone());
    }
    spec
}
