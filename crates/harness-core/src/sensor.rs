use crate::{Action, Execution, Signal, World, error::SensorError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// When in the change lifecycle a sensor runs (DESIGN.md §3, lifecycle distribution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Pre-action — runs before a tool invocation (rare; mostly used by hooks).
    PreAction,
    /// Inside the agent loop, after each action. Cheap & fast only.
    SelfCorrect,
    /// Right before commit / handoff. Heavier checks ok.
    PreCommit,
    /// In CI, after integration. Expensive sensors allowed.
    PostIntegrate,
    /// Long-running runtime monitoring (SLOs, log anomalies, drift).
    Continuous,
}

pub type SensorId = String;

#[async_trait]
pub trait Sensor: Send + Sync + 'static {
    fn id(&self) -> &SensorId;
    fn kind(&self) -> Execution;
    fn stage(&self) -> Stage;
    async fn observe(&self, action: &Action, world: &World) -> Result<Vec<Signal>, SensorError>;
}
