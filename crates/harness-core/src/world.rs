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
    /// Which conversation, and on whose behalf, this world was built for.
    ///
    /// A tool registered once on a long-lived service is one object serving every caller, so a tool
    /// that needs per-conversation state — the stage of a lesson, a draft being assembled — has no way
    /// to tell whose turn it is in. Without this the only options are a single shared slot, which two
    /// simultaneous callers silently corrupt, or building the whole agent per request and giving up
    /// what the service already does.
    ///
    /// `None` outside a served conversation: a CLI run, a test, a one-off loop.
    pub session: Option<SessionRef>,
}

/// Who is talking and in which conversation, for tools that need to keep something per caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    /// The conversation this turn belongs to.
    pub id: String,
    /// Who it is being run for, as the host identifies them. Empty when the host does not authenticate.
    pub actor: String,
    /// This turn, distinct from every other turn of the same conversation — what a tool keys on when
    /// what it holds must not outlive the answer it belongs to.
    pub request: String,
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
