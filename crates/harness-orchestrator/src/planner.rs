//! `Planner` — produces (and re-produces) the DAG.
//!
//! The same interface serves initial planning and **dynamic replanning**:
//! the orchestrator calls `plan` with the goal and everything succeeded so
//! far, and the planner returns either more Jobs to merge or `Done`. This is
//! the feedback edge that turns a static "plan-then-execute" workflow into an
//! agent that adapts its plan to what it learns mid-run.

use crate::dag::PlanDelta;
use crate::job::{Job, JobId, JobResult};
use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlannerError {
    #[error("planner: {0}")]
    Plan(String),
}

/// Decides what Jobs the Run should contain next.
///
/// `?Send`: a planner may itself drive a sub-agent, whose future is not
/// `Send`; the orchestrator runs everything on one thread.
#[async_trait(?Send)]
pub trait Planner {
    async fn plan(
        &self,
        goal: &str,
        succeeded: &[(JobId, JobResult)],
    ) -> Result<PlanDelta, PlannerError>;
}

/// A planner that emits a fixed set of Jobs on the first call and `Done`
/// thereafter. Use it to seed a Run from a precomputed plan while still going
/// through the planner interface (so adding real replanning later is a
/// drop-in swap).
pub struct StaticPlanner {
    jobs: std::sync::Mutex<Option<Vec<Job>>>,
}

impl StaticPlanner {
    pub fn new(jobs: impl IntoIterator<Item = Job>) -> Self {
        Self {
            jobs: std::sync::Mutex::new(Some(jobs.into_iter().collect())),
        }
    }
}

#[async_trait(?Send)]
impl Planner for StaticPlanner {
    async fn plan(
        &self,
        _goal: &str,
        _succeeded: &[(JobId, JobResult)],
    ) -> Result<PlanDelta, PlannerError> {
        match self.jobs.lock().unwrap().take() {
            Some(jobs) => Ok(PlanDelta::Add(jobs)),
            None => Ok(PlanDelta::Done),
        }
    }
}
