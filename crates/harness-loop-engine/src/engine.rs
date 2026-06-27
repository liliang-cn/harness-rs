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
use async_trait::async_trait;
use harness_core::{Memory, MemoryEntry, Model, SubagentStatus, Task, Tool, ToolRisk};
use harness_loop::{Subagent, SubagentReport, SubagentSpec};
use harness_sandbox::{NullSandbox, Sandbox};
use std::path::PathBuf;
use std::sync::Arc;

/// Evidence that an auto-approved action was handed off to its executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionReceipt {
    /// Action kind that was executed, e.g. `"commit"` or `"open-pr"`.
    pub kind: String,
    /// Human-readable execution summary.
    pub summary: String,
}

impl ActionReceipt {
    pub fn new(kind: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            summary: summary.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ActionError {
    #[error("action executor: {0}")]
    Exec(String),
}

/// Carries out a verified, auto-approved action.
///
/// Gates decide whether a proposal is allowed. Executors do the side effect:
/// create a PR, commit a worktree, post a comment, apply a patch, or hand the
/// proposal to any project-specific system. The engine ships conservative
/// defaults; production loops should install an executor that matches their
/// action kinds.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    async fn execute(
        &self,
        spec: &LoopSpec,
        action: &ProposedAction,
        world: &mut harness_core::World,
    ) -> Result<ActionReceipt, ActionError>;
}

/// Default executor: records that a gate approved the action but performs no
/// external side effect. This keeps `LoopEngine::new` safe while making the
/// missing production handoff visible in the round report.
pub struct ApprovalOnlyExecutor;

#[async_trait]
impl ActionExecutor for ApprovalOnlyExecutor {
    async fn execute(
        &self,
        spec: &LoopSpec,
        action: &ProposedAction,
        _world: &mut harness_core::World,
    ) -> Result<ActionReceipt, ActionError> {
        Ok(ActionReceipt::new(
            action.kind.clone(),
            format!(
                "loop `{}` auto-approved `{}`; no external action executor configured",
                spec.name, action.kind
            ),
        ))
    }
}

/// Wrap a synchronous callback as an [`ActionExecutor`]. Use this for
/// application-specific handoffs without defining a bespoke type.
pub struct CallbackActionExecutor<F>(pub F)
where
    F: Fn(
            &LoopSpec,
            &ProposedAction,
            &mut harness_core::World,
        ) -> Result<ActionReceipt, ActionError>
        + Send
        + Sync;

#[async_trait]
impl<F> ActionExecutor for CallbackActionExecutor<F>
where
    F: Fn(
            &LoopSpec,
            &ProposedAction,
            &mut harness_core::World,
        ) -> Result<ActionReceipt, ActionError>
        + Send
        + Sync,
{
    async fn execute(
        &self,
        spec: &LoopSpec,
        action: &ProposedAction,
        world: &mut harness_core::World,
    ) -> Result<ActionReceipt, ActionError> {
        (self.0)(spec, action, world)
    }
}

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
    pub action: Option<ActionReceipt>,
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
        if let Some(a) = &self.action {
            s.push_str(&format!("action: {} — {}\n", a.kind, a.summary));
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
    action_executor: Arc<dyn ActionExecutor>,
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
            action_executor: Arc::new(ApprovalOnlyExecutor),
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
    pub fn with_action_executor(mut self, e: Arc<dyn ActionExecutor>) -> Self {
        self.action_executor = e;
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
        let maker = with_tools_for_level(maker, &self.maker_tools, level);
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
        let checker = with_tools_for_level(checker, &self.checker_tools, level);
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

        let (outcome, action_receipt) = match (&decision, level) {
            // L1 never acts — it reports, regardless of the gate verdict.
            (_, LoopLevel::L1Report) => (RoundOutcome::Reported, None),
            (GateDecision::AutoProceed, _) => {
                match self
                    .action_executor
                    .execute(&self.spec, &proposed, &mut handle.world)
                    .await
                {
                    Ok(receipt) => (RoundOutcome::Proceeded, Some(receipt)),
                    Err(e) => {
                        return self.failed(
                            format!("action executor failed: {e}"),
                            &budget,
                            Some(maker_report),
                            Some(checker_report),
                        );
                    }
                }
            }
            (GateDecision::Escalate { reason }, _) => (
                RoundOutcome::Escalated {
                    reason: reason.clone(),
                },
                None,
            ),
        };

        RoundReport {
            loop_name: self.spec.name.clone(),
            intent: self.spec.intent.clone(),
            level,
            maker: Some(maker_report),
            checker: Some(checker_report),
            decision: Some(decision),
            action: action_receipt,
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
            action: None,
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
            action: None,
            input_tokens: budget.input_tokens,
            output_tokens: budget.output_tokens,
            outcome: RoundOutcome::BudgetExhausted { limit },
        }
    }
}

fn dyn_model(m: &Arc<dyn Model>) -> harness_core::DynModel {
    harness_core::DynModel(m.clone())
}

fn with_tools_for_level(
    mut spec: SubagentSpec,
    tools: &[Arc<dyn Tool>],
    level: LoopLevel,
) -> SubagentSpec {
    for t in tools {
        if level == LoopLevel::L1Report && !l1_tool_allowed(t.risk()) {
            tracing::info!(
                subagent = %spec.name,
                tool = %t.name(),
                risk = ?t.risk(),
                "loop-engine: skipping mutating tool for L1 loop"
            );
            continue;
        }
        spec = spec.with_tool(t.clone());
    }
    spec
}

fn l1_tool_allowed(risk: ToolRisk) -> bool {
    matches!(risk, ToolRisk::ReadOnly | ToolRisk::Network)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AllowlistGate, TokenBudget};
    use async_trait::async_trait;
    use harness_core::{MemoryError, Model, ToolError, ToolResult, ToolSchema, World};
    use harness_models::{MockModel, MockResponse};
    use serde_json::{Value, json};
    use std::sync::{
        Arc as StdArc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn spec(level: LoopLevel) -> LoopSpec {
        LoopSpec::new("test-loop", "keep the test loop honest", level)
            .with_maker_prompt("make a report")
            .with_checker_prompt("verify the report")
            .with_action_kind("commit")
    }

    fn model(resps: impl IntoIterator<Item = MockResponse>) -> Arc<MockModel> {
        Arc::new(MockModel::new().script_many(resps))
    }

    #[derive(Clone)]
    struct TestTool {
        schema: ToolSchema,
        risk: ToolRisk,
    }

    impl TestTool {
        fn new(name: &str, risk: ToolRisk) -> Arc<Self> {
            Arc::new(Self {
                schema: ToolSchema {
                    name: name.into(),
                    description: "test tool".into(),
                    input: json!({"type": "object"}),
                },
                risk,
            })
        }
    }

    #[async_trait]
    impl Tool for TestTool {
        fn name(&self) -> &str {
            &self.schema.name
        }

        fn schema(&self) -> &ToolSchema {
            &self.schema
        }

        fn risk(&self) -> ToolRisk {
            self.risk
        }

        async fn invoke(&self, _args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                ok: true,
                content: json!({"ok": true}),
                trace: None,
            })
        }
    }

    #[derive(Default)]
    struct TestMemory {
        entries: Mutex<Vec<MemoryEntry>>,
    }

    impl TestMemory {
        fn with_entry(entry: MemoryEntry) -> Self {
            Self {
                entries: Mutex::new(vec![entry]),
            }
        }

        fn entries(&self) -> Vec<MemoryEntry> {
            self.entries.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Memory for TestMemory {
        async fn recall(&self, _query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .take(k)
                .cloned()
                .collect())
        }

        async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
            self.entries.lock().unwrap().push(entry);
            Ok(())
        }
    }

    #[tokio::test]
    async fn l1_filters_mutating_tools_before_subagent_context() {
        let model = model([
            MockResponse::text("maker report"),
            MockResponse::text("checker report"),
        ]);
        let engine = LoopEngine::new(spec(LoopLevel::L1Report), model.clone() as Arc<dyn Model>)
            .with_maker_tool(TestTool::new("read", ToolRisk::ReadOnly))
            .with_maker_tool(TestTool::new("write", ToolRisk::Destructive))
            .with_checker_tool(TestTool::new("web", ToolRisk::Network))
            .with_checker_tool(TestTool::new("format", ToolRisk::Idempotent));

        let report = engine.run_once().await;

        assert_eq!(report.outcome, RoundOutcome::Reported);
        let calls = model.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tools_available, vec!["read"]);
        assert_eq!(calls[1].tools_available, vec!["web"]);
        assert!(calls[0].task_description.contains("READ-ONLY"));
    }

    #[tokio::test]
    async fn l3_allowlisted_verified_round_proceeds_quietly() {
        let model = model([
            MockResponse::text("maker produced patch").with_usage(10, 5),
            MockResponse::text("verified clean").with_usage(7, 3),
        ]);
        let engine = LoopEngine::new(spec(LoopLevel::L3Unattended), model as Arc<dyn Model>)
            .with_gate(Arc::new(AllowlistGate::new(["commit"])));

        let report = engine.run_once().await;

        assert_eq!(report.outcome, RoundOutcome::Proceeded);
        assert!(matches!(report.decision, Some(GateDecision::AutoProceed)));
        let action = report.action.as_ref().expect("action receipt");
        assert_eq!(action.kind, "commit");
        assert!(
            action
                .summary
                .contains("no external action executor configured")
        );
        assert!(!report.should_deliver());
        assert_eq!(report.input_tokens, 17);
        assert_eq!(report.output_tokens, 8);
    }

    #[tokio::test]
    async fn auto_proceed_invokes_custom_action_executor() {
        let model = model([
            MockResponse::text("maker produced patch"),
            MockResponse::text("verified clean"),
        ]);
        let calls = StdArc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let executor = CallbackActionExecutor(move |spec, action, world| {
            seen.fetch_add(1, Ordering::SeqCst);
            assert_eq!(spec.name, "test-loop");
            assert_eq!(action.kind, "commit");
            assert_eq!(world.repo.root, PathBuf::from("."));
            Ok(ActionReceipt::new("commit", "created commit abc123"))
        });
        let engine = LoopEngine::new(spec(LoopLevel::L3Unattended), model as Arc<dyn Model>)
            .with_gate(Arc::new(AllowlistGate::new(["commit"])))
            .with_action_executor(Arc::new(executor));

        let report = engine.run_once().await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(report.outcome, RoundOutcome::Proceeded);
        assert_eq!(
            report.action,
            Some(ActionReceipt::new("commit", "created commit abc123"))
        );
        assert!(report.render().contains("action: commit"));
    }

    #[tokio::test]
    async fn action_executor_failure_fails_round() {
        let model = model([
            MockResponse::text("maker produced patch"),
            MockResponse::text("verified clean"),
        ]);
        let executor = CallbackActionExecutor(|_, _, _| Err(ActionError::Exec("boom".into())));
        let engine = LoopEngine::new(spec(LoopLevel::L3Unattended), model as Arc<dyn Model>)
            .with_gate(Arc::new(AllowlistGate::new(["commit"])))
            .with_action_executor(Arc::new(executor));

        let report = engine.run_once().await;

        match &report.outcome {
            RoundOutcome::Failed { error } => {
                assert!(error.contains("action executor failed"));
                assert!(error.contains("boom"));
            }
            other => panic!("expected action failure, got {other:?}"),
        }
        assert!(report.action.is_none());
        assert!(report.should_deliver());
    }

    #[tokio::test]
    async fn budget_exhaustion_after_maker_skips_checker() {
        let model = model([
            MockResponse::text("maker spent too much").with_usage(4, 3),
            MockResponse::text("checker should not run"),
        ]);
        let low_budget = TokenBudget::iters(4).with_max_total_tokens(5);
        let spec = spec(LoopLevel::L2Assisted).with_budget(low_budget);
        let engine = LoopEngine::new(spec, model.clone() as Arc<dyn Model>);

        let report = engine.run_once().await;

        assert_eq!(
            report.outcome,
            RoundOutcome::BudgetExhausted {
                limit: BudgetLimit::Total
            }
        );
        assert!(report.maker.is_some());
        assert!(report.checker.is_none());
        assert_eq!(model.call_count(), 1);
    }

    #[tokio::test]
    async fn memory_state_is_recalled_and_round_is_recorded() {
        let model = model([
            MockResponse::text("maker used prior state"),
            MockResponse::text("checker verified"),
        ]);
        let memory = Arc::new(TestMemory::with_entry(
            MemoryEntry::new("prior loop state").with_tags(["test-loop", "loop-state"]),
        ));
        let engine = LoopEngine::new(spec(LoopLevel::L1Report), model.clone() as Arc<dyn Model>)
            .with_memory(memory.clone());

        let report = engine.run_once().await;

        assert_eq!(report.outcome, RoundOutcome::Reported);
        assert!(
            model.calls()[0]
                .task_description
                .contains("State from previous rounds:\n- prior loop state")
        );
        let entries = memory.entries();
        assert_eq!(entries.len(), 2);
        let recorded = entries.last().unwrap();
        assert!(recorded.content.starts_with("reported"));
        assert_eq!(recorded.source.as_deref(), Some("loop:test-loop"));
        assert!(recorded.tags.iter().any(|t| t == "loop-state"));
    }
}
