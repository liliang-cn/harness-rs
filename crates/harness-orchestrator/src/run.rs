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
    /// Every Job's outcome. DAG order not implied.
    pub jobs: Vec<JobReport>,
}

/// One Job's outcome, as a reader of the run needs it.
///
/// `error` is here because without it a failed run is unreadable: a job shows
/// as `DeadLettered` with an empty body and the reason — which the job had all
/// along in `last_error` — never leaves the graph. Measured on a real run, a
/// `grep` job died on an invalid regex and the report said only "DeadLettered".
/// `visits` is here for the same reason, one level up: a loop that hit its cap
/// looks identical to one that failed once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobReport {
    pub id: String,
    pub state: JobState,
    /// What it produced, if it succeeded.
    pub result: Option<String>,
    /// Why it failed, if it did.
    pub error: Option<String>,
    /// Attempts of the latest entry, and how many times it was entered.
    pub attempts: u32,
    pub visits: u32,
}

impl RunReport {
    pub fn succeeded(&self) -> usize {
        self.jobs
            .iter()
            .filter(|j| j.state == JobState::Succeeded)
            .count()
    }
    pub fn dead_lettered(&self) -> usize {
        self.jobs
            .iter()
            .filter(|j| j.state == JobState::DeadLettered)
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
        for j in &self.jobs {
            s.push_str(&format!("  - {}: {}", j.id, j.state.label()));
            if j.visits > 1 {
                // A loop that hit its cap otherwise reads like a single failure.
                s.push_str(&format!(" (entered {}×)", j.visits));
            }
            // Char-safe truncation (text may contain multi-byte chars / emoji).
            let brief = |t: &str| t.trim().chars().take(80).collect::<String>();
            if let Some(t) = &j.result {
                s.push_str(&format!(" — {}", brief(t)));
            }
            // The reason comes last and always: a failed job with an empty line
            // is the report saying nothing at exactly the moment it matters.
            if let Some(e) = &j.error {
                s.push_str(&format!(" — failed: {}", brief(e)));
            }
            s.push('\n');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobState;

    #[test]
    fn render_truncates_on_char_boundary_with_multibyte_text() {
        // A job result whose 80th byte lands inside a multi-byte char must not
        // panic when rendered (regression: byte-slicing `&t[..80]`).
        let long = format!("🔄 {}", "更".repeat(100));
        let report = RunReport {
            run_id: "r".into(),
            goal: "g".into(),
            state: RunState::Completed,
            spent_tokens: 0,
            jobs: vec![JobReport {
                id: "j".into(),
                state: JobState::Succeeded,
                result: Some(long),
                error: None,
                attempts: 1,
                visits: 1,
            }],
        };
        let out = report.render(); // must not panic
        assert!(out.contains("j: succeeded"));
    }
}
