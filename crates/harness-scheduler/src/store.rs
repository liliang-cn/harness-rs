//! Job persistence. `Job` is the unit of scheduled work; `JobStore` is the
//! pluggable backend (default `FileJobStore`, a single JSON-array file written
//! atomically — same posture as `harness_context::FileMemory`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

fn default_true() -> bool { true }

/// One scheduled agent job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Job {
    pub id: String,
    pub name: String,
    /// "daily 08:00" | "weekly mon 09:30" | "every 15m" (parsed by harness_daemon::Schedule).
    pub schedule: String,
    /// The agent task to run at fire time.
    pub prompt: String,
    /// Channel key (matched against a registered Channel), e.g. "stdout" | "email".
    pub channel: String,
    /// Channel-specific recipient (email address, chat id, …).
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run_ms: Option<i64>,
    #[serde(default)]
    pub next_run_ms: Option<i64>,
    pub created_ms: i64,
}

impl Job {
    pub fn new(name: impl Into<String>, schedule: impl Into<String>, prompt: impl Into<String>, channel: impl Into<String>, created_ms: i64) -> Self {
        Self {
            id: gen_id(created_ms),
            name: name.into(),
            schedule: schedule.into(),
            prompt: prompt.into(),
            channel: channel.into(),
            target: None,
            enabled: true,
            last_run_ms: None,
            next_run_ms: None,
            created_ms,
        }
    }
    pub fn with_target(mut self, t: Option<String>) -> Self { self.target = t; self }
    pub fn with_next_run(mut self, ms: Option<i64>) -> Self { self.next_run_ms = ms; self }
}

/// Short-ish unique id: created-ms + a process-global counter.
fn gen_id(created_ms: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("job-{created_ms}-{}", SEQ.fetch_add(1, Ordering::SeqCst))
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JobError {
    #[error("job io: {0}")] Io(String),
    #[error("job serde: {0}")] Serde(String),
}

#[async_trait]
pub trait JobStore: Send + Sync + 'static {
    async fn add(&self, job: &Job) -> Result<(), JobError>;
    async fn list(&self) -> Result<Vec<Job>, JobError>;
    async fn get(&self, id: &str) -> Result<Option<Job>, JobError>;
    async fn remove(&self, id: &str) -> Result<bool, JobError>;
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, JobError>;
    async fn record_run(&self, id: &str, last_run_ms: i64, next_run_ms: Option<i64>) -> Result<(), JobError>;
}

/// JSON-array file backend. All jobs in one file; mutations read-all → modify →
/// atomic rewrite. `Mutex<()>` serializes writers.
pub struct FileJobStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl FileJobStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, JobError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| JobError::Io(e.to_string()))?;
            }
        }
        Ok(Self { path, write_lock: Mutex::new(()) })
    }

    fn read_all(&self) -> Result<Vec<Job>, JobError> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| JobError::Serde(e.to_string())),
            _ => Ok(Vec::new()),
        }
    }

    fn write_all(&self, jobs: &[Job]) -> Result<(), JobError> {
        let _g = self.write_lock.lock().map_err(|e| JobError::Io(e.to_string()))?;
        let buf = serde_json::to_string_pretty(jobs).map_err(|e| JobError::Serde(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, buf).map_err(|e| JobError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| JobError::Io(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl JobStore for FileJobStore {
    async fn add(&self, job: &Job) -> Result<(), JobError> {
        let mut jobs = self.read_all()?;
        jobs.push(job.clone());
        self.write_all(&jobs)
    }
    async fn list(&self) -> Result<Vec<Job>, JobError> { self.read_all() }
    async fn get(&self, id: &str) -> Result<Option<Job>, JobError> {
        Ok(self.read_all()?.into_iter().find(|j| j.id == id))
    }
    async fn remove(&self, id: &str) -> Result<bool, JobError> {
        let mut jobs = self.read_all()?;
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        let removed = jobs.len() != before;
        if removed { self.write_all(&jobs)?; }
        Ok(removed)
    }
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, JobError> {
        let mut jobs = self.read_all()?;
        let mut found = false;
        for j in &mut jobs { if j.id == id { j.enabled = enabled; found = true; } }
        if found { self.write_all(&jobs)?; }
        Ok(found)
    }
    async fn record_run(&self, id: &str, last_run_ms: i64, next_run_ms: Option<i64>) -> Result<(), JobError> {
        let mut jobs = self.read_all()?;
        for j in &mut jobs { if j.id == id { j.last_run_ms = Some(last_run_ms); j.next_run_ms = next_run_ms; } }
        self.write_all(&jobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-jobstore-{}-{n}.json", std::process::id()))
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let p = tmp();
        let store = FileJobStore::open(&p).unwrap();
        let job = Job::new("digest", "daily 08:00", "summarize", "stdout", 1000).with_next_run(Some(2000));
        let id = job.id.clone();
        store.add(&job).await.unwrap();

        assert_eq!(store.list().await.unwrap().len(), 1);
        assert_eq!(store.get(&id).await.unwrap().unwrap().name, "digest");

        assert!(store.set_enabled(&id, false).await.unwrap());
        assert!(!store.get(&id).await.unwrap().unwrap().enabled);

        store.record_run(&id, 5000, Some(9000)).await.unwrap();
        let j = store.get(&id).await.unwrap().unwrap();
        assert_eq!(j.last_run_ms, Some(5000));
        assert_eq!(j.next_run_ms, Some(9000));

        assert!(store.remove(&id).await.unwrap());
        assert!(store.list().await.unwrap().is_empty());
        assert!(!store.remove(&id).await.unwrap());

        let _ = std::fs::remove_file(&p);
    }
}
