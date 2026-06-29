//! `Run` — one user goal, executed as a DAG of Jobs, plus its top-level
//! state machine and budget.

use crate::dag::Dag;
use crate::job::JobState;
use serde::{Deserialize, Serialize};

pub type RunId = String;

/// Lifecycle of a whole Run. Mirrors the article's Run state model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Created,
    Planning,
    Executing,
    /// All runnable Jobs are blocked on something external (a retry backoff,
    /// or — with a planner — awaiting the next replan).
    Waiting,
    Aggregating,
    Completed,
    Failed,
    Cancelled,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunState::Completed | RunState::Failed | RunState::Cancelled
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            RunState::Created => "created",
            RunState::Planning => "planning",
            RunState::Executing => "executing",
            RunState::Waiting => "waiting",
            RunState::Aggregating => "aggregating",
            RunState::Completed => "completed",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
        }
    }
}

/// A run-level spend ceiling. The orchestrator tallies every Job's token
/// usage against this and stops the Run if it is exceeded — the cost
/// governance the async-orchestration literature usually leaves out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBudget {
    pub max_total_tokens: Option<u64>,
}

impl RunBudget {
    pub fn unlimited() -> Self {
        Self::default()
    }
    pub fn max_total_tokens(n: u64) -> Self {
        Self {
            max_total_tokens: Some(n),
        }
    }
    /// True if `spent` is over the ceiling.
    pub fn exceeded(&self, spent: u64) -> bool {
        matches!(self.max_total_tokens, Some(m) if spent > m)
    }
}

/// The persistent record of a Run: its goal, state, budget, and DAG. This is
/// what a [`crate::RunStore`] saves and reloads for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub goal: String,
    pub state: RunState,
    pub budget: RunBudget,
    pub dag: Dag,
    pub spent_tokens: u64,
}

impl Run {
    pub fn new(id: impl Into<RunId>, goal: impl Into<String>, dag: Dag) -> Self {
        Self {
            id: id.into(),
            goal: goal.into(),
            state: RunState::Created,
            budget: RunBudget::unlimited(),
            dag,
            spent_tokens: 0,
        }
    }

    pub fn with_budget(mut self, b: RunBudget) -> Self {
        self.budget = b;
        self
    }
}

/// A read-only summary returned when a Run finishes. Suitable for delivery
/// and for aggregating Job results.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub run_id: RunId,
    pub goal: String,
    pub state: RunState,
    pub spent_tokens: u64,
    /// `(job_id, state, result_text)` for every Job, DAG order not implied.
    pub jobs: Vec<(String, JobState, Option<String>)>,
}

impl RunReport {
    pub fn succeeded(&self) -> usize {
        self.jobs
            .iter()
            .filter(|(_, s, _)| *s == JobState::Succeeded)
            .count()
    }
    pub fn dead_lettered(&self) -> usize {
        self.jobs
            .iter()
            .filter(|(_, s, _)| *s == JobState::DeadLettered)
            .count()
    }

    pub fn render(&self) -> String {
        let mut s = format!(
            "Run `{}` [{}] — {} goal: {}\n",
            self.run_id,
            self.state.label(),
            format_args!("{} jobs, {} tokens;", self.jobs.len(), self.spent_tokens),
            self.goal,
        );
        for (id, st, text) in &self.jobs {
            s.push_str(&format!("  - {id}: {}", st.label()));
            if let Some(t) = text {
                let t = t.trim();
                let short = if t.len() > 80 { &t[..80] } else { t };
                s.push_str(&format!(" — {short}"));
            }
            s.push('\n');
        }
        s
    }
}
