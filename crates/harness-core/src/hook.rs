use crate::{Event, World};
use serde::{Deserialize, Serialize};

/// What a hook tells the runtime to do after firing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookOutcome {
    /// Continue normally.
    Allow,
    /// Refuse the action; surface `reason` to the model.
    Deny { reason: String },
    /// Replace some part of the event payload (event-specific).
    Mutate(serde_json::Value),
    /// Inject text into the active context.
    Inject(String),
}

pub trait Hook: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn matches(&self, ev: &Event<'_>) -> bool;
    fn fire(&self, ev: &Event<'_>, world: &mut World) -> HookOutcome;
}
