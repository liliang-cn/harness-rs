//! Concrete `World` runtime impls.

use async_trait::async_trait;
use harness_core::{Clock, KvStore, ProcessOutput, ProcessRunner};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Real-time system clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Subprocess runner backed by `tokio::process::Command`.
pub struct TokioRunner;

#[async_trait]
impl ProcessRunner for TokioRunner {
    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<ProcessOutput> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        let out = cmd.output().await?;
        Ok(ProcessOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }
}

/// Thread-safe in-memory key-value store.
pub struct InMemoryKv {
    inner: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryKv {
    pub fn new() -> Self { Self { inner: Mutex::new(HashMap::new()) } }
}

impl Default for InMemoryKv {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl KvStore for InMemoryKv {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.lock().ok()?.get(key).cloned()
    }
    async fn set(&self, key: &str, value: Vec<u8>) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(key.to_string(), value);
        }
    }
    async fn delete(&self, key: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.remove(key);
        }
    }
}
