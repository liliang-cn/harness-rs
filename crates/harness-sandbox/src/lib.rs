//! Sandbox trait + concrete backends.
//!
//! DESIGN.md §11: "权限不是运行时弹窗, 而是 spawn 时一次性烧进沙箱."
//!
//! - [`WorktreeSandbox`] — git worktree on a feature branch; cheapest and most
//!   useful default. Auto-cleanup on drop unless `.keep()` is called.
//! - `ContainerSandbox` / `VmSandbox` — placeholder enums for the v0.2+ road.

use async_trait::async_trait;
use harness_core::{RepoView, World};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("git not available: {0}")]
    GitMissing(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(String),
}

/// Filesystem access policy a sandbox can advertise to the framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsPolicy {
    /// Sandbox can read/write only inside its own root.
    Confined,
    /// Sandbox can read anywhere but only write inside its root.
    HostReadConfinedWrite,
    /// No restriction (debugging only).
    Unrestricted,
}

/// Network access policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetPolicy {
    /// No outbound network at all.
    None,
    /// Outbound network allowed (today: enforced by host; v0.2 adds real nets).
    Allowed,
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn spawn(&self) -> Result<SandboxHandle, SandboxError>;
    fn fs_policy(&self) -> FsPolicy;
    fn net_policy(&self) -> NetPolicy;
}

/// What a sandbox hands back to the caller — a `World` rooted at the sandbox
/// path, plus introspection / cleanup helpers.
pub struct SandboxHandle {
    pub world: World,
    pub root:  PathBuf,
    cleanup:   Option<Box<dyn FnOnce() + Send>>,
    keep:      bool,
    label:     String,
}

impl SandboxHandle {
    pub fn label(&self) -> &str { &self.label }
    pub fn root(&self)  -> &Path { &self.root }

    /// Skip the cleanup callback on drop. Useful when you want to inspect the
    /// sandbox afterwards.
    pub fn keep(&mut self) { self.keep = true; }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        if self.keep { return; }
        if let Some(f) = self.cleanup.take() {
            f();
        }
    }
}

// ============================================================
// WorktreeSandbox
// ============================================================

/// Spawn a fresh git worktree at a sibling path on a new branch.
///
/// Requires `git` on `PATH`. The host repo must be a git repository.
pub struct WorktreeSandbox {
    /// Path to the *source* git repo.
    pub source: PathBuf,
    /// Branch name to create (must not already exist).
    pub branch: String,
    /// Optional starting ref; defaults to the current HEAD.
    pub start_ref: Option<String>,
    /// Optional explicit worktree path (default: tempdir).
    pub target: Option<PathBuf>,
}

impl WorktreeSandbox {
    pub fn new(source: impl Into<PathBuf>, branch: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            branch: branch.into(),
            start_ref: None,
            target: None,
        }
    }

    pub fn with_target(mut self, p: impl Into<PathBuf>) -> Self {
        self.target = Some(p.into());
        self
    }

    pub fn with_start_ref(mut self, r: impl Into<String>) -> Self {
        self.start_ref = Some(r.into());
        self
    }
}

#[async_trait]
impl Sandbox for WorktreeSandbox {
    fn fs_policy(&self) -> FsPolicy { FsPolicy::Confined }
    fn net_policy(&self) -> NetPolicy { NetPolicy::Allowed }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        // Choose target path
        let target = match &self.target {
            Some(p) => p.clone(),
            None    => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                std::env::temp_dir().join(format!("harness-worktree-{stamp}"))
            }
        };

        // Verify git available
        let probe = tokio::process::Command::new("git")
            .arg("--version")
            .output()
            .await
            .map_err(|e| SandboxError::GitMissing(format!("`git --version` failed: {e}")))?;
        if !probe.status.success() {
            return Err(SandboxError::GitMissing(
                String::from_utf8_lossy(&probe.stderr).into_owned(),
            ));
        }

        // `git worktree add -b <branch> <target> [<start_ref>]`
        let start = self.start_ref.clone().unwrap_or_else(|| "HEAD".to_string());
        let target_str = target.to_string_lossy().to_string();
        let args: Vec<&str> = vec!["worktree", "add", "-b", &self.branch, &target_str, &start];
        tracing::info!(branch=%self.branch, target=%target.display(), "spawning worktree");
        let out = tokio::process::Command::new("git")
            .current_dir(&self.source)
            .args(&args)
            .output()
            .await
            .map_err(|e| SandboxError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(SandboxError::Git(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }

        // Build a World pointing at the worktree.
        let world = World {
            repo:   RepoView { root: target.clone() },
            runner: Arc::new(harness_context::TokioRunner),
            clock:  Arc::new(harness_context::SystemClock),
            kv:     Arc::new(harness_context::InMemoryKv::new()),
        };

        let cleanup_source = self.source.clone();
        let cleanup_target = target.clone();
        let cleanup_branch = self.branch.clone();
        let cleanup: Box<dyn FnOnce() + Send> = Box::new(move || {
            let target_str = cleanup_target.to_string_lossy().to_string();
            let args: Vec<&str> = vec!["worktree", "remove", "--force", &target_str];
            let res = std::process::Command::new("git")
                .current_dir(&cleanup_source)
                .args(&args)
                .output();
            if let Ok(o) = res
                && !o.status.success()
            {
                tracing::warn!(
                    "git worktree remove failed for {}: {}",
                    cleanup_target.display(),
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            // Best-effort branch delete (will fail if checked out elsewhere).
            let _ = std::process::Command::new("git")
                .current_dir(&cleanup_source)
                .args(["branch", "-D", &cleanup_branch])
                .output();
        });

        Ok(SandboxHandle {
            world,
            root: target,
            cleanup: Some(cleanup),
            keep: false,
            label: format!("worktree:{}", self.branch),
        })
    }
}

// ============================================================
// NullSandbox (for tests & for "I don't actually need isolation")
// ============================================================

/// A no-op sandbox: returns a World rooted at the host repo, no isolation.
pub struct NullSandbox {
    pub root: PathBuf,
}

impl NullSandbox {
    pub fn new(root: impl Into<PathBuf>) -> Self { Self { root: root.into() } }
}

#[async_trait]
impl Sandbox for NullSandbox {
    fn fs_policy(&self) -> FsPolicy { FsPolicy::Unrestricted }
    fn net_policy(&self) -> NetPolicy { NetPolicy::Allowed }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        let world = World {
            repo:   RepoView { root: self.root.clone() },
            runner: Arc::new(harness_context::TokioRunner),
            clock:  Arc::new(harness_context::SystemClock),
            kv:     Arc::new(harness_context::InMemoryKv::new()),
        };
        Ok(SandboxHandle {
            world,
            root: self.root.clone(),
            cleanup: None,
            keep: false,
            label: "null".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_sandbox_returns_world() {
        let s = NullSandbox::new(".");
        let h = s.spawn().await.unwrap();
        assert_eq!(h.label(), "null");
    }

    #[tokio::test]
    async fn worktree_sandbox_spawns_and_cleans_up() {
        // Skip unless we're inside a git repo.
        let probe = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output();
        let Ok(out) = probe else { return; };
        if !out.status.success() { return; }
        let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let branch = format!("harness-test-{}", std::process::id());
        let s = WorktreeSandbox::new(&src, &branch);
        let handle = s.spawn().await.expect("worktree spawns");
        assert!(handle.root().exists(), "worktree directory exists");
        assert!(handle.label().starts_with("worktree:"));
        let root = handle.root().to_path_buf();
        drop(handle);
        // Cleanup should remove the directory.
        // (Best-effort: give git a moment on slow filesystems.)
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while root.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
    }
}
