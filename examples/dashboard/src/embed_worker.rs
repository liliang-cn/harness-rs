//! Background worker that fills in `notes.embedding` for rows where it's
//! NULL. Runs as a tokio task started from `main`.
//!
//! Strategy: every `tick`, pull up to `batch_size` pending rows, embed them
//! in one Gemini batchEmbedContents call, write back. Sleep `idle_pause`
//! when nothing's pending so we don't hot-spin on an empty queue.
//!
//! Crash-safety: if the process dies mid-batch, embedding stays NULL on
//! disk; next launch picks the rows back up via the partial index.

use harness_core::Embedder;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct EmbedWorker {
    pub db_path: PathBuf,
    pub embedder: Arc<dyn Embedder>,
    pub batch_size: u32,
    pub idle_pause: Duration,
    pub busy_pause: Duration,
}

impl EmbedWorker {
    pub fn spawn(self) {
        tokio::spawn(async move {
            tracing::info!(
                handle = self.embedder.handle(),
                dim = self.embedder.dim(),
                batch = self.batch_size,
                "embed worker started"
            );
            loop {
                match self.tick().await {
                    Ok(0) => tokio::time::sleep(self.idle_pause).await,
                    Ok(n) => {
                        tracing::debug!(embedded = n, "embed batch ok");
                        tokio::time::sleep(self.busy_pause).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "embed batch failed; backing off");
                        tokio::time::sleep(Duration::from_secs(15)).await;
                    }
                }
            }
        });
    }

    async fn tick(&self) -> anyhow::Result<usize> {
        // Open and close the DB connection on each pass — rusqlite::Connection
        // is !Send, so we can't hold it across the .await for the network call.
        let pending = {
            let db = crate::db::Db::open(&self.db_path)?;
            db.pending_embeds(self.batch_size)?
        };
        if pending.is_empty() {
            return Ok(0);
        }

        // Concat title + body for embedding context. Title alone is too sparse;
        // body alone misses theme. Use "title\n\nbody" — same shape Gemini was
        // trained on.
        let texts: Vec<String> = pending
            .iter()
            .map(|p| {
                if p.title.is_empty() {
                    p.body.clone()
                } else {
                    format!("{}\n\n{}", p.title, p.body)
                }
            })
            .collect();
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let vectors = self.embedder.embed(&refs).await?;
        if vectors.len() != pending.len() {
            anyhow::bail!(
                "embedder returned {} vectors for {} inputs",
                vectors.len(),
                pending.len()
            );
        }

        let dim = self.embedder.dim();
        let db = crate::db::Db::open(&self.db_path)?;
        for (p, v) in pending.iter().zip(vectors.iter()) {
            db.write_embedding(&p.id, dim, v)?;
        }
        Ok(pending.len())
    }
}
