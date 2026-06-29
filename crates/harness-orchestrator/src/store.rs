//! `RunStore` — persists Run + Job state so a Run can resume after a crash.
//!
//! Persistence is what separates "fancy async callbacks" from a recoverable
//! task system: the store is the source of truth. The orchestrator saves the
//! whole [`Run`] after every state transition; on resume, Jobs caught mid-
//! flight (`Running`/`Queued`/`Retrying`) are reset to `Pending` and the Run
//! continues from its succeeded results.

use crate::run::Run;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("run store io: {0}")]
    Io(String),
    #[error("run store serde: {0}")]
    Serde(String),
}

#[async_trait]
pub trait RunStore: Send + Sync {
    async fn save(&self, run: &Run) -> Result<(), StoreError>;
    async fn load(&self, run_id: &str) -> Result<Option<Run>, StoreError>;
}

/// In-memory store — for tests and ephemeral Runs.
#[derive(Default)]
pub struct InMemoryRunStore {
    runs: Mutex<HashMap<String, Run>>,
}

impl InMemoryRunStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RunStore for InMemoryRunStore {
    async fn save(&self, run: &Run) -> Result<(), StoreError> {
        self.runs
            .lock()
            .unwrap()
            .insert(run.id.clone(), run.clone());
        Ok(())
    }
    async fn load(&self, run_id: &str) -> Result<Option<Run>, StoreError> {
        Ok(self.runs.lock().unwrap().get(run_id).cloned())
    }
}

/// One JSON file per Run under `dir` (`<dir>/<run_id>.json`).
pub struct FileRunStore {
    dir: PathBuf,
}

impl FileRunStore {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(Self { dir })
    }
    fn path(&self, run_id: &str) -> PathBuf {
        // run_id is caller-controlled; keep it to a safe filename.
        let safe: String = run_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }
}

#[async_trait]
impl RunStore for FileRunStore {
    async fn save(&self, run: &Run) -> Result<(), StoreError> {
        let json = serde_json::to_vec_pretty(run).map_err(|e| StoreError::Serde(e.to_string()))?;
        std::fs::write(self.path(&run.id), json).map_err(|e| StoreError::Io(e.to_string()))
    }
    async fn load(&self, run_id: &str) -> Result<Option<Run>, StoreError> {
        let p = self.path(run_id);
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p).map_err(|e| StoreError::Io(e.to_string()))?;
        let run = serde_json::from_slice(&bytes).map_err(|e| StoreError::Serde(e.to_string()))?;
        Ok(Some(run))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::Dag;
    use crate::job::Job;

    #[tokio::test]
    async fn file_store_roundtrips() {
        let dir = std::env::temp_dir().join(format!("harness-orch-{}", std::process::id()));
        let store = FileRunStore::open(&dir).unwrap();
        let run = Run::new("run-1", "goal", Dag::from_jobs([Job::new("a", "x")]));
        store.save(&run).await.unwrap();
        let back = store.load("run-1").await.unwrap().unwrap();
        assert_eq!(back.id, "run-1");
        assert_eq!(back.dag.len(), 1);
        assert!(store.load("missing").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
