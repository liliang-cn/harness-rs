//! Semantic search over a user's notes.
//!
//! v1 strategy: linear scan over all embedded rows. For a personal note
//! corpus (<10k notes × 768 floats = ~30 MB peak) this is microseconds in
//! Rust and saves us from carrying an HNSW / Annoy index. We re-evaluate
//! when one user crosses ~50k notes.

use crate::db::{Db, Note, NoteEmbedding};
use harness_core::{Embedder, EmbedError, dot, l2_normalize};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    #[serde(flatten)]
    pub note: Note,
    /// Cosine similarity in [-1, 1]. Higher = closer.
    pub score: f32,
    /// `true` if this row matched the query as a fallback substring grep
    /// (because semantic search couldn't run — e.g. no embeddings yet).
    pub via_grep: bool,
}

pub async fn semantic_search(
    db_path: &Path,
    user_id: &str,
    embedder: &Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    space: Option<&str>,
) -> anyhow::Result<Vec<Hit>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    // Always fetch the corpus first so we can fall back to grep if the
    // embedder is down.
    let corpus: Vec<NoteEmbedding> = {
        let db = Db::open(db_path)?;
        db.list_embeddings(user_id, space)?
    };

    // Try semantic first.
    let q_vec = match embed_query(embedder, q).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "embed query failed; falling back to grep");
            None
        }
    };

    let mut hits: Vec<Hit> = Vec::new();
    if let Some(mut q_vec) = q_vec {
        l2_normalize(&mut q_vec);
        for NoteEmbedding { note, mut embedding } in corpus.into_iter() {
            if embedding.len() != q_vec.len() {
                // Dimension mismatch (older row from a previous model).
                // Skip; the worker will re-embed when it notices.
                continue;
            }
            l2_normalize(&mut embedding);
            let score = dot(&q_vec, &embedding);
            hits.push(Hit {
                note,
                score,
                via_grep: false,
            });
        }
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    }

    // If semantic returned nothing usable (no embeddings yet for this user
    // OR all dim-mismatched), fall back to a case-insensitive substring scan
    // over the FULL note table — including rows that haven't been embedded yet.
    if hits.is_empty() {
        let db = Db::open(db_path)?;
        let all = db.list_recent_notes(user_id, space, 5000)?;
        let needle = q.to_lowercase();
        for note in all {
            let hay = format!("{}\n{}", note.title, note.body).to_lowercase();
            if hay.contains(&needle) {
                hits.push(Hit { note, score: 0.0, via_grep: true });
            }
        }
    }

    hits.truncate(top_k);
    Ok(hits)
}

async fn embed_query(embedder: &Arc<dyn Embedder>, q: &str) -> Result<Vec<f32>, EmbedError> {
    let mut out = embedder.embed(&[q]).await?;
    out.pop()
        .ok_or_else(|| EmbedError::Provider("empty result".into()))
}
