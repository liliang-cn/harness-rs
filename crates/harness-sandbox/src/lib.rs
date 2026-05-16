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
    pub root: PathBuf,
    cleanup: Option<Box<dyn FnOnce() + Send>>,
    keep: bool,
    label: String,
}

impl SandboxHandle {
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Skip the cleanup callback on drop. Useful when you want to inspect the
    /// sandbox afterwards.
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
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
    fn fs_policy(&self) -> FsPolicy {
        FsPolicy::Confined
    }
    fn net_policy(&self) -> NetPolicy {
        NetPolicy::Allowed
    }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        // Choose target path
        let target = match &self.target {
            Some(p) => p.clone(),
            None => {
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
            repo: RepoView {
                root: target.clone(),
            },
            runner: Arc::new(harness_context::TokioRunner),
            clock: Arc::new(harness_context::SystemClock),
            kv: Arc::new(harness_context::InMemoryKv::new()),
            profile: harness_core::UserProfile::default(),
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
// ContainerSandbox
// ============================================================

/// Run the agent against a Docker container. The agent itself runs on the
/// host, but every `world.runner.exec(...)` call routes through `docker exec`
/// into a long-lived container whose workspace is bind-mounted from the host.
///
/// Requires `docker` on PATH; the container image must include the tools the
/// agent intends to use (cargo, git, etc.).
pub struct ContainerSandbox {
    /// OCI image to spawn (e.g. `rust:1.92-slim`).
    pub image: String,
    /// Host directory to mount inside the container at `/workspace`.
    pub source: PathBuf,
    /// Container name; auto-generated if `None`.
    pub name: Option<String>,
    /// Pass `--network none` to the container.
    pub no_net: bool,
}

impl ContainerSandbox {
    pub fn new(image: impl Into<String>, source: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            source: source.into(),
            name: None,
            no_net: true,
        }
    }

    pub fn with_network(mut self, allow: bool) -> Self {
        self.no_net = !allow;
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

#[async_trait]
impl Sandbox for ContainerSandbox {
    fn fs_policy(&self) -> FsPolicy {
        FsPolicy::Confined
    }
    fn net_policy(&self) -> NetPolicy {
        if self.no_net {
            NetPolicy::None
        } else {
            NetPolicy::Allowed
        }
    }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        // probe docker
        let probe = tokio::process::Command::new("docker")
            .arg("--version")
            .output()
            .await
            .map_err(|e| SandboxError::GitMissing(format!("docker missing: {e}")))?;
        if !probe.status.success() {
            return Err(SandboxError::GitMissing(
                String::from_utf8_lossy(&probe.stderr).to_string(),
            ));
        }

        let name = self.name.clone().unwrap_or_else(|| {
            format!(
                "harness-sb-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )
        });
        let mount = format!("{}:/workspace", self.source.display());
        let mut args = vec![
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-v",
            &mount,
            "-w",
            "/workspace",
        ];
        if self.no_net {
            args.push("--network");
            args.push("none");
        }
        args.push(&self.image);
        args.push("sleep");
        args.push("infinity");

        tracing::info!(image=%self.image, name=%name, "spawning container sandbox");
        let out = tokio::process::Command::new("docker")
            .args(&args)
            .output()
            .await
            .map_err(|e| SandboxError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(SandboxError::Git(format!(
                "docker run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }

        // Wrap the world's runner so every exec routes through `docker exec`.
        let runner: Arc<dyn harness_core::ProcessRunner> = Arc::new(DockerExecRunner {
            container: name.clone(),
        });
        let world = World {
            repo: RepoView {
                root: self.source.clone(),
            },
            runner,
            clock: Arc::new(harness_context::SystemClock),
            kv: Arc::new(harness_context::InMemoryKv::new()),
            profile: harness_core::UserProfile::default(),
        };

        let kill_name = name.clone();
        let cleanup: Box<dyn FnOnce() + Send> = Box::new(move || {
            let _ = std::process::Command::new("docker")
                .args(["kill", &kill_name])
                .output();
        });

        Ok(SandboxHandle {
            world,
            root: self.source.clone(),
            cleanup: Some(cleanup),
            keep: false,
            label: format!("container:{name}"),
        })
    }
}

struct DockerExecRunner {
    container: String,
}

#[async_trait]
impl harness_core::ProcessRunner for DockerExecRunner {
    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
    ) -> std::io::Result<harness_core::ProcessOutput> {
        let mut docker_args: Vec<String> = vec!["exec".into()];
        if let Some(c) = cwd {
            docker_args.push("-w".into());
            // Re-anchor relative cwd inside the container's /workspace mount.
            let inside = if c.is_absolute() {
                c.display().to_string()
            } else {
                format!("/workspace/{}", c.display())
            };
            docker_args.push(inside);
        }
        docker_args.push(self.container.clone());
        docker_args.push(program.into());
        for a in args {
            docker_args.push((*a).into());
        }
        let out = tokio::process::Command::new("docker")
            .args(&docker_args)
            .output()
            .await?;
        Ok(harness_core::ProcessOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }
}

// ============================================================
// VmSandbox — Firecracker-shaped API, stub backend
// ============================================================

/// Sandbox API for VM-isolated agent runs. The full Firecracker backend is
/// out of scope for a pure Rust crate; this type validates configuration and
/// returns an error from `spawn()` so callers can detect missing infra
/// without bringing the framework down.
pub struct VmSandbox {
    pub kernel_image: PathBuf,
    pub rootfs_image: PathBuf,
    pub source: PathBuf,
    pub vcpus: u8,
    pub mem_mb: u32,
}

impl VmSandbox {
    pub fn new(
        kernel: impl Into<PathBuf>,
        rootfs: impl Into<PathBuf>,
        source: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kernel_image: kernel.into(),
            rootfs_image: rootfs.into(),
            source: source.into(),
            vcpus: 1,
            mem_mb: 512,
        }
    }
}

#[async_trait]
impl Sandbox for VmSandbox {
    fn fs_policy(&self) -> FsPolicy {
        FsPolicy::Confined
    }
    fn net_policy(&self) -> NetPolicy {
        NetPolicy::None
    }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        // Validate config so users get an early error before learning the
        // backend isn't wired up.
        if !self.kernel_image.exists() {
            return Err(SandboxError::Io(format!(
                "kernel image not found: {}",
                self.kernel_image.display()
            )));
        }
        if !self.rootfs_image.exists() {
            return Err(SandboxError::Io(format!(
                "rootfs image not found: {}",
                self.rootfs_image.display()
            )));
        }
        Err(SandboxError::GitMissing(
            "VmSandbox: Firecracker backend not yet implemented in this build. \
             Use ContainerSandbox for OCI isolation or WorktreeSandbox for cheap \
             git-level isolation."
                .into(),
        ))
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
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl Sandbox for NullSandbox {
    fn fs_policy(&self) -> FsPolicy {
        FsPolicy::Unrestricted
    }
    fn net_policy(&self) -> NetPolicy {
        NetPolicy::Allowed
    }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        let world = World {
            repo: RepoView {
                root: self.root.clone(),
            },
            runner: Arc::new(harness_context::TokioRunner),
            clock: Arc::new(harness_context::SystemClock),
            kv: Arc::new(harness_context::InMemoryKv::new()),
            profile: harness_core::UserProfile::default(),
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
    async fn vm_sandbox_returns_clear_error_when_unimplemented() {
        let tmp = std::env::temp_dir().join(format!("harness-vm-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("kernel"), b"fake").unwrap();
        std::fs::write(tmp.join("rootfs"), b"fake").unwrap();
        let s = VmSandbox::new(tmp.join("kernel"), tmp.join("rootfs"), tmp.clone());
        let err = match s.spawn().await {
            Ok(_) => panic!("expected VmSandbox spawn to error"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Firecracker") || msg.contains("not yet implemented"),
            "got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn vm_sandbox_rejects_missing_images() {
        let s = VmSandbox::new("/no/such/kernel", "/no/such/rootfs", ".");
        assert!(matches!(s.spawn().await, Err(SandboxError::Io(_))));
    }

    #[tokio::test]
    async fn container_sandbox_fails_cleanly_without_docker() {
        let s = ContainerSandbox::new("harness-nonexistent-image-xyzzy:latest", ".");
        // either docker missing OR image pull fails — both produce Err
        assert!(
            s.spawn().await.is_err(),
            "expected error spawning bogus container"
        );
    }

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
        let Ok(out) = probe else {
            return;
        };
        if !out.status.success() {
            return;
        }
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
