use crate::{Event, World};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

/// `inventory` slot for compile-time hook registration via `#[hook]`.
pub struct HookEntry {
    pub factory: fn() -> Arc<dyn Hook>,
}

inventory::collect!(HookEntry);

/// Enumerate every `#[hook]`-registered hook.
pub fn iter_macro_hooks() -> impl Iterator<Item = Arc<dyn Hook>> {
    inventory::iter::<HookEntry>().map(|e| (e.factory)())
}
