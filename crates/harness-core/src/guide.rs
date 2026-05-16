use crate::{Context, Execution, World, error::GuideError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What kind of work this guide applies to. Determines when `apply` runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum GuideScope {
    /// Always inject for every task.
    Always,
    /// Only when the task description matches one of the given globs / regexes.
    TaskMatches(Vec<String>),
    /// Only when the world's repo contains files matching `pattern`.
    FilesMatch { pattern: String },
}

impl GuideScope {
    /// True if this guide should run for the given task.
    pub fn matches(&self, task: &crate::Task) -> bool {
        match self {
            GuideScope::Always => true,
            GuideScope::TaskMatches(patterns) => {
                patterns.iter().any(|p| task.description.contains(p))
            }
            GuideScope::FilesMatch { .. } => true,
        }
    }
}

pub type GuideId = String;

#[async_trait]
pub trait Guide: Send + Sync + 'static {
    fn id(&self) -> &GuideId;
    fn kind(&self) -> Execution;
    fn scope(&self) -> &GuideScope;
    async fn apply(&self, ctx: &mut Context, world: &World) -> Result<(), GuideError>;
}

pub struct GuideEntry {
    pub factory: fn() -> Arc<dyn Guide>,
}

inventory::collect!(GuideEntry);

pub fn iter_macro_guides() -> impl Iterator<Item = Arc<dyn Guide>> {
    inventory::iter::<GuideEntry>().map(|e| (e.factory)())
}
