//! `JobRunner` — how a single Job actually executes.
//!
//! The orchestrator is decoupled from execution: it schedules Jobs and
//! tracks state, but delegates "do the work" to a `JobRunner`. The default
//! [`SubagentJobRunner`] runs each Job as an isolated `harness-loop`
//! sub-agent; tests inject a trivial closure runner. Every Job gets a fresh
//! `World` from a factory, which both sidesteps the `&mut World` aliasing
//! that would otherwise block concurrency and gives each Job worker-style
//! isolation.

use crate::job::{Job, JobId, JobResult};
use async_trait::async_trait;
use harness_core::{Model, SubagentStatus, Task, Tool, World};
use harness_loop::{Subagent, SubagentSpec};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JobError {
    #[error("job runner: {0}")]
    Run(String),
    #[error("job blocked: {0}")]
    Blocked(String),
}

/// Executes one Job. `?Send` because a sub-agent future is not `Send`.
#[async_trait(?Send)]
pub trait JobRunner {
    async fn run(&self, job: &Job, deps: &[(JobId, JobResult)]) -> Result<JobResult, JobError>;
}

/// Builds a fresh `World` for each Job. Defaults to `default_world(root)`;
/// swap in a sandbox-backed factory for stronger isolation.
pub type WorldFactory = Arc<dyn Fn() -> World + Send + Sync>;

/// The production runner: each Job becomes an isolated sub-agent.
pub struct SubagentJobRunner {
    model: Arc<dyn Model>,
    tools: Vec<Arc<dyn Tool>>,
    world_factory: WorldFactory,
    max_iters: u32,
}

impl SubagentJobRunner {
    /// Run Jobs against `model`, each in a fresh `default_world(root)`.
    pub fn new(model: Arc<dyn Model>, root: impl Into<std::path::PathBuf>) -> Self {
        let root = root.into();
        Self {
            model,
            tools: Vec::new(),
            world_factory: Arc::new(move || harness_context::default_world(root.clone())),
            max_iters: 12,
        }
    }

    pub fn with_tool(mut self, t: Arc<dyn Tool>) -> Self {
        self.tools.push(t);
        self
    }
    pub fn with_max_iters(mut self, n: u32) -> Self {
        self.max_iters = n;
        self
    }
    /// Override how each Job's `World` is built (e.g. a worktree sandbox).
    pub fn with_world_factory(mut self, f: WorldFactory) -> Self {
        self.world_factory = f;
        self
    }

    fn task_description(&self, job: &Job, deps: &[(JobId, JobResult)]) -> String {
        if deps.is_empty() {
            return job.prompt.clone();
        }
        let mut d = String::from("Results from upstream jobs you depend on:\n");
        for (id, r) in deps {
            d.push_str(&format!("--- {id} ---\n{}\n", r.text.trim()));
        }
        d.push_str("\nYour task:\n");
        d.push_str(&job.prompt);
        d
    }
}

#[async_trait(?Send)]
impl JobRunner for SubagentJobRunner {
    async fn run(&self, job: &Job, deps: &[(JobId, JobResult)]) -> Result<JobResult, JobError> {
        let mut world = (self.world_factory)();
        let mut spec = SubagentSpec::new(
            job.id.clone(),
            Task {
                description: self.task_description(job, deps),
                source: None,
                deadline: None,
            },
        )
        .with_max_iters(self.max_iters);
        for t in &self.tools {
            spec = spec.with_tool(t.clone());
        }
        let sub = Subagent::new(harness_core::DynModel(self.model.clone()), spec);
        let report = sub
            .run(&mut world)
            .await
            .map_err(|e| JobError::Run(e.to_string()))?;
        match report.status {
            SubagentStatus::Done | SubagentStatus::DoneWithConcerns => Ok(JobResult {
                text: report.text.unwrap_or_default(),
                input_tokens: report.usage.input_tokens as u64,
                output_tokens: report.usage.output_tokens as u64,
            }),
            SubagentStatus::Blocked => Err(JobError::Blocked(
                report
                    .text
                    .unwrap_or_else(|| "sub-agent budget exhausted".into()),
            )),
            SubagentStatus::NeedsContext => Err(JobError::Blocked(
                report
                    .text
                    .unwrap_or_else(|| "sub-agent needs more context".into()),
            )),
            _ => Err(JobError::Blocked("sub-agent did not complete".into())),
        }
    }
}
