//! Subagent: an isolated agent loop spawned from a parent.
//!
//! Per DESIGN.md §8, a subagent has:
//! - independent `Context`
//! - restricted tool / sensor / guide set
//! - its own iteration budget
//!
//! A subagent reports back via [`SubagentStatus`] (the Superpowers
//! convention: Done / DoneWithConcerns / Blocked / NeedsContext).

use crate::{AgentLoop, Outcome};
use harness_core::{
    Guide, HarnessError, Model, Sensor, SubagentStatus, Task, Tool, World,
};
use std::sync::Arc;

/// What a subagent needs to run.
pub struct SubagentSpec {
    pub name:      String,
    pub task:      Task,
    pub tools:     Vec<Arc<dyn Tool>>,
    pub guides:    Vec<Arc<dyn Guide>>,
    pub sensors:   Vec<Arc<dyn Sensor>>,
    pub max_iters: u32,
}

impl SubagentSpec {
    pub fn new(name: impl Into<String>, task: Task) -> Self {
        Self {
            name: name.into(),
            task,
            tools:   Vec::new(),
            guides:  Vec::new(),
            sensors: Vec::new(),
            max_iters: 12,
        }
    }

    pub fn with_tool(mut self, t: Arc<dyn Tool>) -> Self {
        self.tools.push(t);
        self
    }

    pub fn with_guide(mut self, g: Arc<dyn Guide>) -> Self {
        self.guides.push(g);
        self
    }

    pub fn with_sensor(mut self, s: Arc<dyn Sensor>) -> Self {
        self.sensors.push(s);
        self
    }

    pub fn with_max_iters(mut self, n: u32) -> Self {
        self.max_iters = n;
        self
    }
}

/// A subagent's structured report back to the parent.
#[derive(Debug, Clone)]
pub struct SubagentReport {
    pub name:   String,
    pub status: SubagentStatus,
    pub text:   Option<String>,
    pub iters:  u32,
}

/// Bind a `Model` to a `SubagentSpec` and run it.
pub struct Subagent<M: Model> {
    pub spec:   SubagentSpec,
    pub loop_:  AgentLoop<M>,
}

impl<M: Model> Subagent<M> {
    pub fn new(model: M, spec: SubagentSpec) -> Self {
        let mut loop_ = AgentLoop::new(model);
        for t in &spec.tools   { loop_ = loop_.with_tool(t.clone()); }
        for g in &spec.guides  { loop_ = loop_.with_guide(g.clone()); }
        for s in &spec.sensors { loop_ = loop_.with_sensor(s.clone()); }
        Self { spec, loop_ }
    }

    pub async fn run(self, world: &mut World) -> Result<SubagentReport, HarnessError> {
        let name = self.spec.name.clone();
        let max  = self.spec.max_iters;
        let task = self.spec.task.clone();
        let outcome = self.loop_.run_with_max_iters(task, world, max).await?;
        let report = match outcome {
            Outcome::Done { text, iters, .. } => SubagentReport {
                name,
                status: SubagentStatus::Done,
                text,
                iters,
            },
            Outcome::BudgetExhausted { iters, .. } => SubagentReport {
                name,
                status: SubagentStatus::Blocked,
                text: None,
                iters,
            },
        };
        tracing::info!(
            subagent = %report.name,
            status = ?report.status,
            iters = report.iters,
            "subagent completed"
        );
        Ok(report)
    }
}
