use crate::{Context, error::CompactError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// The 5 progressive compaction stages (DESIGN.md §9 — borrowed from Claude Code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CompactionStage {
    /// > 60% — trim redundant tool results, keep recent full turns.
    BudgetReduce,
    /// > 70% — drop stale file reads, keep path + hash.
    Snip,
    /// > 80% — summarise older conversation segments with a cheaper model.
    Microcompact,
    /// > 90% — collapse all file reads into a single inventory + key excerpts.
    ContextCollapse,
    /// > 95% — rewrite the entire conversation in compressed form with the main model.
    AutoCompact,
}

impl CompactionStage {
    /// Stages, in order. Higher stages imply running all lower stages first.
    pub const ALL: [CompactionStage; 5] = [
        CompactionStage::BudgetReduce,
        CompactionStage::Snip,
        CompactionStage::Microcompact,
        CompactionStage::ContextCollapse,
        CompactionStage::AutoCompact,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub used: u32,
    pub window: u32,
}

impl Budget {
    pub fn ratio(&self) -> f32 {
        if self.window == 0 {
            0.0
        } else {
            self.used as f32 / self.window as f32
        }
    }

    /// Which stages must run, given current usage. Returns lowest → highest.
    pub fn required_stages(&self) -> Vec<CompactionStage> {
        let r = self.ratio();
        let mut out = Vec::new();
        if r > 0.60 {
            out.push(CompactionStage::BudgetReduce);
        }
        if r > 0.70 {
            out.push(CompactionStage::Snip);
        }
        if r > 0.80 {
            out.push(CompactionStage::Microcompact);
        }
        if r > 0.90 {
            out.push(CompactionStage::ContextCollapse);
        }
        if r > 0.95 {
            out.push(CompactionStage::AutoCompact);
        }
        out
    }
}

#[async_trait]
pub trait Compactor: Send + Sync + 'static {
    fn budget(&self, ctx: &Context) -> Budget;
    async fn compact(&self, stage: CompactionStage, ctx: &mut Context) -> Result<(), CompactError>;
}
