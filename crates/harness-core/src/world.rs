use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Runtime view of the filesystem / repo the agent operates on.
#[derive(Debug, Clone)]
pub struct RepoView {
    pub root: PathBuf,
}

/// Things the agent and its sensors can do that aren't covered by a Tool.
///
/// This is intentionally tiny — most work flows through Tools. `Clone` is cheap
/// (all fields are `Arc` / small `Copy`-ish data), which lets the loop dispatch
/// several read-only tools concurrently, each against its own `World`.
#[derive(Clone)]
pub struct World {
    pub repo: RepoView,
    pub runner: Arc<dyn ProcessRunner>,
    pub clock: Arc<dyn Clock>,
    pub kv: Arc<dyn KvStore>,
    /// Ambient information about who's using the agent (name, timezone, locale,
    /// preferences). Loaded once at world construction. See [`crate::UserProfile`].
    pub profile: crate::UserProfile,
}

#[async_trait]
pub trait ProcessRunner: Send + Sync + 'static {
    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> std::io::Result<ProcessOutput>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait Clock: Send + Sync + 'static {
    /// Unix milliseconds since epoch.
    fn now_ms(&self) -> i64;
}

#[async_trait]
pub trait KvStore: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Option<Vec<u8>>;
    async fn set(&self, key: &str, value: Vec<u8>);
    async fn delete(&self, key: &str);
}
