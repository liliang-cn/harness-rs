//! Cross-run memory: the agent records the actions it applied and recalls
//! them on the next run so it doesn't re-propose what was already done. This
//! is the durable spine that turns repeated runs into a coherent operation.

use harness_core::{Memory, MemoryEntry};

const RECALL_QUERY: &str = "ops applied action reorder markdown pause product";

/// Recall prior applied actions as a bulleted string for prompt injection.
pub async fn recall_prior(mem: &dyn Memory) -> String {
    match mem.recall(RECALL_QUERY, 12).await {
        Ok(entries) if !entries.is_empty() => entries
            .iter()
            .map(|e| format!("- {}", e.content))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Persist the actions applied this run.
pub async fn remember(mem: &dyn Memory, applied: &[String]) {
    for a in applied {
        let entry = MemoryEntry::new(format!("applied {a}"))
            .with_tags(["ops", "applied"])
            .with_source("ecommerce-ops-agent");
        if let Err(e) = mem.write(entry).await {
            tracing::warn!(error = %e, "failed to persist ops memory");
        }
    }
}
