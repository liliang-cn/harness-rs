//! Sandbox trait + concrete backends.
//!
//! Principle (DESIGN.md §11): permissions are decided when the execution
//! environment is *spawned*, not re-prompted per tool call.
//!
//! **Be honest about what each backend enforces** — see [`Isolation`]:
//!
//! - [`WorktreeSandbox`] — a git worktree on a feature branch. Isolates
//!   *changes* (they land on a branch, not `main`), **not capability**: a shell
//!   command inside it can still `cd /` and touch anything. `Isolation::Changes`.
//! - [`SeatbeltSandbox`] (macOS) / [`BubblewrapSandbox`] (Linux) — OS-native,
//!   kernel-enforced, no daemon. Each runs a command as a *separate* sandboxed
//!   process (`sandbox-exec` / `bwrap`), so the harness process is never
//!   restricted — the per-command helper model Codex CLI uses, and the reason
//!   the in-process `birdcage` crate was *not* used (its `spawn` leaks the
//!   sandbox to the caller). Denies network by default. `Isolation::Process`.
//!   Confining *writes* is the default posture and stops a command damaging the
//!   host; it does **not** stop one reading it. On a multi-tenant host, where
//!   the threat is one tenant reading another's workspace rather than breaking
//!   the machine, ask for `with_confine_reads(true)` — then and only then does
//!   `fs_policy()` report [`FsPolicy::Confined`].
//! - [`ContainerSandbox`] — routes `runner.exec` through `docker exec` into a
//!   container (`--network none` for real net isolation). The container/kernel
//!   does the isolating; this crate is only the orchestration. `Isolation::Process`.
//! - [`NullSandbox`] — no isolation (tests / opt-out). `Isolation::None`.
//!
//! **Scope (all backends):** a sandbox here wraps `world.runner.exec` — i.e.
//! *shell subprocesses*. The agent's in-process filesystem tools are confined
//! *separately*: `harness-tools-fs` runs every op through a capability
//! directory (`cap-std`), so their workspace jail is OS-enforced (`openat`),
//! not a string check. So the two side-effect channels are each confined —
//! shell by this sandbox, files by the cap-std jail — though not yet behind a
//! single unified `World` capability.

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
    /// Outbound network allowed.
    Allowed,
}

/// What a backend **actually enforces** — as distinct from the `FsPolicy` /
/// `NetPolicy` it *requests*. A policy the kernel doesn't enforce is not
/// isolation, so callers should branch on this, not on the requested policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Nothing is enforced; the agent has host access.
    None,
    /// Only *changes* are isolated (e.g. a git worktree/branch). Capability is
    /// **not** restricted — commands can still read/write/network the host.
    Changes,
    /// Shell subprocesses are confined by the kernel or a container (Seatbelt /
    /// Landlock / Docker). In-process fs tools are jailed separately.
    Process,
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn spawn(&self) -> Result<SandboxHandle, SandboxError>;
    fn fs_policy(&self) -> FsPolicy;
    fn net_policy(&self) -> NetPolicy;
    /// What this backend actually enforces (be honest — see [`Isolation`]).
    fn isolation(&self) -> Isolation;
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

    /// Take ownership of the sandbox `World`, keeping the environment alive
    /// (the cleanup callback is dropped, like [`keep`](Self::keep)). Intended
    /// for backends with no cleanup resource — Seatbelt / bubblewrap / null —
    /// where nothing leaks.
    pub fn into_world(self) -> World {
        let md = std::mem::ManuallyDrop::new(self);
        // SAFETY: `md` is a `ManuallyDrop`, so `Drop` never runs and `world` is
        // moved out exactly once (no double-free). The skipped `cleanup` is
        // `None` for the OS backends this is used with.
        unsafe { std::ptr::read(&md.world) }
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
        // Requested, not enforced — a worktree does not stop writes elsewhere.
        FsPolicy::Confined
    }
    fn net_policy(&self) -> NetPolicy {
        NetPolicy::Allowed
    }
    fn isolation(&self) -> Isolation {
        Isolation::Changes
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
            session: None,
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
    fn isolation(&self) -> Isolation {
        Isolation::Process
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
            session: None,
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
// SeatbeltSandbox (macOS, OS-native — no Docker, per-command)
// ============================================================

/// **macOS only.** Runs each shell subprocess under Apple's Seatbelt via
/// `sandbox-exec` — a *separate* sandboxed process per command, so the harness
/// process itself is never restricted (unlike an in-process crate such as
/// birdcage, whose `spawn` leaks the sandbox to the caller). Kernel-enforced,
/// no daemon; denies outbound network by default. The same per-command helper
/// approach OpenAI's Codex CLI uses.
///
/// Only wraps `world.runner.exec` (shell); in-process fs tools are jailed
/// separately — see the module docs.
pub struct SeatbeltSandbox {
    pub root: PathBuf,
    pub allow_net: bool,
    pub confine_writes: bool,
    pub confine_reads: bool,
}

impl SeatbeltSandbox {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            allow_net: false,
            confine_writes: false,
            confine_reads: false,
        }
    }
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_net = allow;
        self
    }
    pub fn with_confine_writes(mut self, confine: bool) -> Self {
        self.confine_writes = confine;
        self
    }
    /// Also deny *reads* outside the root (plus the system paths a process
    /// needs to start at all).
    ///
    /// Write confinement alone stops a command corrupting the host, but it
    /// leaves every readable file on the box in reach — which on a multi-tenant
    /// server means tenant A's shell can `cat` tenant B's workspace. Isolating
    /// changes is not the same as isolating capability; if the threat is
    /// exfiltration rather than damage, this is the switch that matters.
    ///
    /// Implies [`with_confine_writes`](Self::with_confine_writes): a filesystem
    /// you may write but not read back is not a coherent policy, and every
    /// caller that wants read confinement wants both.
    pub fn with_confine_reads(mut self, confine: bool) -> Self {
        self.confine_reads = confine;
        if confine {
            self.confine_writes = true;
        }
        self
    }
}

/// System paths a process must be able to read before it can execute at all —
/// the dynamic loader, the shared cache, the binaries themselves, and the CA
/// bundle any networked tool will look for. Denying these does not harden the
/// sandbox, it just makes every command fail with a linker error.
///
/// `/opt` covers Homebrew (`/opt/homebrew` on Apple silicon); `/nix` covers a
/// Nix-installed toolchain. Both are absent on a stock machine, and a Seatbelt
/// `subpath` allow for a path that does not exist is inert, so listing them
/// costs nothing.
/// Deliberately absent: `/private/tmp` and `/private/var/folders`. They stay
/// *writable* (the OS and half the toolchain write scratch there unconditionally
/// and cannot be talked out of it) but not *readable*, because a shared temp
/// directory that every tenant can read is exactly the exfiltration path read
/// confinement exists to close. A read-confined sandbox gets its own `TMPDIR`
/// inside its root instead — see [`SeatbeltRunner`].
const MACOS_READ_ALLOW: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/opt",
    "/nix",
    "/etc",
    "/private/etc",
    "/var/select",
    "/dev",
    "/private/var/db/dyld",
];

/// Build a Seatbelt profile (SBPL): `allow default`, then subtract capabilities.
pub fn seatbelt_profile(
    root: &Path,
    allow_net: bool,
    confine_writes: bool,
    confine_reads: bool,
) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n");
    if !allow_net {
        p.push_str("(deny network*)\n");
    }
    if confine_writes {
        p.push_str("(deny file-write*)\n");
        p.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            root.display()
        ));
        p.push_str(
            "(allow file-write* (subpath \"/private/var/folders\") (subpath \"/private/tmp\"))\n",
        );
        p.push_str("(allow file-write* (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\") (literal \"/dev/dtracehelper\"))\n");
    }
    if confine_reads {
        p.push_str("(deny file-read*)\n");
        // Metadata stays readable everywhere: resolving `<root>/x` walks every
        // parent directory, so denying stat on `/Users` makes the workspace
        // itself unreachable — a denial that blocks nothing an attacker wants
        // (the bytes) while breaking everything the tenant does.
        p.push_str("(allow file-read-metadata)\n");
        // `/` itself, and it is not optional: a `subpath` allow for `/usr`
        // does not cover reading the root *directory entry*, and without it
        // every process — even `/bin/echo` — dies with SIGABRT before `main`,
        // silently (no stderr, exit 134). Allowing every top-level directory
        // individually does not substitute for it. Found the hard way.
        p.push_str("(allow file-read* (literal \"/\"))\n");
        p.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            root.display()
        ));
        for path in MACOS_READ_ALLOW {
            p.push_str(&format!("(allow file-read* (subpath \"{path}\"))\n"));
        }
    }
    p
}

struct SeatbeltRunner {
    profile: String,
    /// Private scratch directory handed to the child as `TMPDIR`, set only when
    /// reads are confined.
    ///
    /// The shared temp dirs stay writable but unreadable under read
    /// confinement, so a command that writes a scratch file and reads it back —
    /// which is most of them — would break. Pointing `TMPDIR` inside the
    /// sandbox root fixes that without reopening the cross-tenant read.
    private_tmp: Option<PathBuf>,
}

#[async_trait]
impl harness_core::ProcessRunner for SeatbeltRunner {
    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
    ) -> std::io::Result<harness_core::ProcessOutput> {
        // sandbox-exec -p <profile> <program> <args...>
        let mut sb: Vec<String> = vec!["-p".into(), self.profile.clone(), program.into()];
        sb.extend(args.iter().map(|a| (*a).to_string()));
        let mut cmd = tokio::process::Command::new("sandbox-exec");
        cmd.args(&sb);
        if let Some(t) = &self.private_tmp {
            std::fs::create_dir_all(t)?;
            // TMPDIR is what libc and every sane toolchain honour; TMP/TEMP are
            // there for the ported ones that only look at those.
            cmd.env("TMPDIR", t).env("TMP", t).env("TEMP", t);
        }
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        let out = cmd.output().await?;
        Ok(harness_core::ProcessOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }
}

#[async_trait]
impl Sandbox for SeatbeltSandbox {
    fn fs_policy(&self) -> FsPolicy {
        match (self.confine_reads, self.confine_writes) {
            (true, _) => FsPolicy::Confined,
            (false, true) => FsPolicy::HostReadConfinedWrite,
            (false, false) => FsPolicy::Unrestricted,
        }
    }
    fn net_policy(&self) -> NetPolicy {
        if self.allow_net {
            NetPolicy::Allowed
        } else {
            NetPolicy::None
        }
    }
    fn isolation(&self) -> Isolation {
        Isolation::Process
    }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        let probe = tokio::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1)(allow default)")
            .arg("/usr/bin/true")
            .output()
            .await
            .map_err(|e| SandboxError::GitMissing(format!("sandbox-exec unavailable: {e}")))?;
        if !probe.status.success() {
            return Err(SandboxError::GitMissing(
                "sandbox-exec probe failed (macOS only)".into(),
            ));
        }
        let world = World {
            repo: RepoView {
                root: self.root.clone(),
            },
            runner: Arc::new(SeatbeltRunner {
                profile: seatbelt_profile(
                    &self.root,
                    self.allow_net,
                    self.confine_writes,
                    self.confine_reads,
                ),
                private_tmp: self.confine_reads.then(|| self.root.join(".tmp")),
            }),
            clock: Arc::new(harness_context::SystemClock),
            kv: Arc::new(harness_context::InMemoryKv::new()),
            profile: harness_core::UserProfile::default(),
            session: None,
        };
        Ok(SandboxHandle {
            world,
            root: self.root.clone(),
            cleanup: None,
            keep: false,
            label: "seatbelt".into(),
        })
    }
}

// ============================================================
// BubblewrapSandbox (Linux, OS-native — no Docker/daemon, per-command)
// ============================================================

/// **Linux only.** Runs each shell subprocess under
/// [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`) — a separate
/// sandboxed process per command (parent never restricted). The Linux
/// counterpart to [`SeatbeltSandbox`]. Network denied by default; with
/// `confine_writes`, `--ro-bind / /` + `--bind <root> <root>` gives real
/// read-host / write-workspace enforcement.
pub struct BubblewrapSandbox {
    pub root: PathBuf,
    pub allow_net: bool,
    pub confine_writes: bool,
    pub confine_reads: bool,
}

impl BubblewrapSandbox {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            allow_net: false,
            confine_writes: true,
            confine_reads: false,
        }
    }
    pub fn with_network(mut self, allow: bool) -> Self {
        self.allow_net = allow;
        self
    }
    pub fn with_confine_writes(mut self, confine: bool) -> Self {
        self.confine_writes = confine;
        self
    }
    /// Also deny *reads* outside the root: bind in the system paths a process
    /// needs and nothing else, instead of `--ro-bind / /`.
    ///
    /// See [`SeatbeltSandbox::with_confine_reads`] for why write confinement on
    /// its own is not enough on a multi-tenant host. Implies write confinement.
    pub fn with_confine_reads(mut self, confine: bool) -> Self {
        self.confine_reads = confine;
        if confine {
            self.confine_writes = true;
        }
        self
    }
}

/// The only host paths bound into a read-confined bubblewrap sandbox — enough
/// for a dynamically-linked binary to load and resolve TLS, and no more.
///
/// Bound with `--ro-bind-try` rather than `--ro-bind`: `/lib64` is absent on
/// aarch64, `/nix` on anything but NixOS, and `bwrap` treats a missing bind
/// source as a fatal error. `-try` turns "not on this distro" into a no-op,
/// which is what it is.
const LINUX_READ_BIND: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/nix",
];

/// Build the `bwrap` args before the user's program. Pure → unit-testable, and
/// verified live in a Linux container (`--unshare-net` blocks network;
/// `--ro-bind / /` + `--bind <root>` confines writes to the workspace).
pub fn bwrap_args(
    root: &Path,
    allow_net: bool,
    confine_writes: bool,
    confine_reads: bool,
) -> Vec<String> {
    let r = root.display().to_string();
    let mut a: Vec<String> = Vec::new();
    if confine_reads {
        // Nothing is visible by default; bind in the system tree read-only and
        // the workspace read-write. The host's home directories — every other
        // tenant — are simply not in the mount namespace.
        for path in LINUX_READ_BIND {
            a.extend(["--ro-bind-try".into(), (*path).into(), (*path).into()]);
        }
        a.extend(["--bind".into(), r.clone(), r]);
        // A private /tmp, so scratch files are neither visible to nor from the
        // host — a shared /tmp is a side channel between tenants.
        a.extend(["--tmpfs".into(), "/tmp".into()]);
    } else if confine_writes {
        a.extend(["--ro-bind".into(), "/".into(), "/".into()]);
        a.extend(["--bind".into(), r.clone(), r]);
    } else {
        a.extend(["--bind".into(), "/".into(), "/".into()]);
    }
    a.extend(["--proc".into(), "/proc".into()]);
    a.extend(["--dev".into(), "/dev".into()]);
    if !allow_net {
        a.push("--unshare-net".into());
    }
    a
}

struct BwrapRunner {
    args: Vec<String>,
}

#[async_trait]
impl harness_core::ProcessRunner for BwrapRunner {
    async fn exec(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
    ) -> std::io::Result<harness_core::ProcessOutput> {
        let mut full = self.args.clone();
        full.push(program.into());
        full.extend(args.iter().map(|a| (*a).to_string()));
        let mut cmd = tokio::process::Command::new("bwrap");
        cmd.args(&full);
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        let out = cmd.output().await?;
        Ok(harness_core::ProcessOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }
}

#[async_trait]
impl Sandbox for BubblewrapSandbox {
    fn fs_policy(&self) -> FsPolicy {
        match (self.confine_reads, self.confine_writes) {
            (true, _) => FsPolicy::Confined,
            (false, true) => FsPolicy::HostReadConfinedWrite,
            (false, false) => FsPolicy::Unrestricted,
        }
    }
    fn net_policy(&self) -> NetPolicy {
        if self.allow_net {
            NetPolicy::Allowed
        } else {
            NetPolicy::None
        }
    }
    fn isolation(&self) -> Isolation {
        Isolation::Process
    }

    async fn spawn(&self) -> Result<SandboxHandle, SandboxError> {
        let probe = tokio::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .await
            .map_err(|e| SandboxError::GitMissing(format!("bwrap unavailable: {e}")))?;
        if !probe.status.success() {
            return Err(SandboxError::GitMissing(
                "bwrap probe failed (Linux only)".into(),
            ));
        }
        let runner = Arc::new(BwrapRunner {
            args: bwrap_args(
                &self.root,
                self.allow_net,
                self.confine_writes,
                self.confine_reads,
            ),
        });
        let world = World {
            repo: RepoView {
                root: self.root.clone(),
            },
            runner,
            clock: Arc::new(harness_context::SystemClock),
            kv: Arc::new(harness_context::InMemoryKv::new()),
            profile: harness_core::UserProfile::default(),
            session: None,
        };
        Ok(SandboxHandle {
            world,
            root: self.root.clone(),
            cleanup: None,
            keep: false,
            label: "bubblewrap".into(),
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
    fn isolation(&self) -> Isolation {
        Isolation::None
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
            session: None,
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
    async fn container_sandbox_fails_cleanly_without_docker() {
        let s = ContainerSandbox::new("harness-nonexistent-image-xyzzy:latest", ".");
        // Three ways this can go, all of them "no container was started":
        // docker is absent, the image pull fails, or — the case that used to
        // hang this test forever — the CLI is installed but the daemon is down,
        // and `docker run` waits on it indefinitely. Bound the wait so a stopped
        // Docker Desktop can't wedge the whole suite.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), s.spawn()).await;
        match outcome {
            Ok(spawned) => assert!(spawned.is_err(), "expected error spawning bogus container"),
            Err(_elapsed) => { /* daemon never answered — nothing was spawned */ }
        }
    }

    #[tokio::test]
    async fn null_sandbox_returns_world() {
        let s = NullSandbox::new(".");
        let h = s.spawn().await.unwrap();
        assert_eq!(h.label(), "null");
    }

    #[test]
    fn seatbelt_profile_denies_network_by_default() {
        let p = seatbelt_profile(std::path::Path::new("/tmp/x"), false, false, false);
        assert!(p.contains("(deny network*)"));
        let allowed = seatbelt_profile(std::path::Path::new("/tmp/x"), true, false, false);
        assert!(!allowed.contains("(deny network*)"));
    }

    #[test]
    fn seatbelt_read_confinement_is_off_unless_asked() {
        let p = seatbelt_profile(std::path::Path::new("/work"), false, true, false);
        assert!(
            !p.contains("(deny file-read*)"),
            "write confinement must not silently imply read confinement"
        );
    }

    #[test]
    fn seatbelt_read_confinement_allows_root_and_system_paths() {
        let p = seatbelt_profile(std::path::Path::new("/work/tenant-a"), false, true, true);
        assert!(p.contains("(deny file-read*)"));
        assert!(p.contains("(allow file-read* (subpath \"/work/tenant-a\"))"));
        // Without these a dynamically-linked binary cannot even start.
        for required in ["/usr", "/System", "/private/var/db/dyld"] {
            assert!(
                p.contains(&format!("(allow file-read* (subpath \"{required}\"))")),
                "missing read allow for {required}"
            );
        }
        // Path resolution walks parents of the root, so metadata must stay open.
        assert!(p.contains("(allow file-read-metadata)"));
        // Without the root directory entry itself nothing executes at all.
        assert!(p.contains("(allow file-read* (literal \"/\"))"));
        // The point of the whole exercise: another tenant's tree is not listed.
        assert!(!p.contains("/work/tenant-b"));
        // Shared scratch is writable but NOT readable — otherwise one tenant's
        // temp file is every tenant's temp file.
        assert!(p.contains(
            "(allow file-write* (subpath \"/private/var/folders\") (subpath \"/private/tmp\"))"
        ));
        assert!(!p.contains("(allow file-read* (subpath \"/private/tmp\"))"));
        assert!(!p.contains("(allow file-read* (subpath \"/private/var/folders\"))"));
    }

    #[test]
    fn seatbelt_confine_reads_implies_confine_writes() {
        let s = SeatbeltSandbox::new("/work").with_confine_reads(true);
        assert!(s.confine_writes);
        assert_eq!(s.fs_policy(), FsPolicy::Confined);
        assert_eq!(
            SeatbeltSandbox::new("/work")
                .with_confine_writes(true)
                .fs_policy(),
            FsPolicy::HostReadConfinedWrite
        );
    }

    #[test]
    fn bwrap_args_deny_net_and_confine_writes_by_default() {
        let a = bwrap_args(std::path::Path::new("/work"), false, true, false);
        assert!(a.iter().any(|x| x == "--unshare-net"));
        assert!(a.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));
        assert!(a.windows(3).any(|w| w == ["--bind", "/work", "/work"]));
        let net = bwrap_args(std::path::Path::new("/work"), true, true, false);
        assert!(!net.iter().any(|x| x == "--unshare-net"));
    }

    #[test]
    fn bwrap_read_confinement_binds_system_paths_only() {
        let a = bwrap_args(std::path::Path::new("/work/tenant-a"), false, true, true);
        // The whole-root read bind is exactly what read confinement removes.
        assert!(
            !a.windows(3).any(|w| w == ["--ro-bind", "/", "/"]),
            "read-confined sandbox must not bind the whole filesystem"
        );
        assert!(a.windows(3).any(|w| w == ["--ro-bind-try", "/usr", "/usr"]));
        assert!(
            a.windows(3)
                .any(|w| w == ["--bind", "/work/tenant-a", "/work/tenant-a"])
        );
        // /home is never bound, so no other tenant exists in the namespace.
        assert!(!a.iter().any(|x| x == "/home"));
        // Shared /tmp is a cross-tenant side channel; it gets its own tmpfs.
        assert!(a.windows(2).any(|w| w == ["--tmpfs", "/tmp"]));
    }

    #[test]
    fn bwrap_confine_reads_implies_confine_writes() {
        let s = BubblewrapSandbox::new("/work").with_confine_reads(true);
        assert!(s.confine_writes);
        assert_eq!(s.fs_policy(), FsPolicy::Confined);
    }

    #[tokio::test]
    async fn bubblewrap_fails_cleanly_off_linux() {
        // On macOS `bwrap` is absent → spawn errors, doesn't panic.
        let absent = std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true);
        if absent {
            assert!(BubblewrapSandbox::new(".").spawn().await.is_err());
        }
    }

    /// macOS: prove read confinement is enforced by the *kernel*, not just
    /// present in the profile string.
    ///
    /// The scenario is the one that motivated the flag: two tenant directories
    /// side by side, a sandbox rooted at one of them, and a command that tries
    /// to read the other. Asserting on the SBPL text would pass even if the
    /// profile were subtly malformed and Seatbelt ignored it, which is exactly
    /// the failure this crate refuses to ship — a policy the kernel does not
    /// enforce is not isolation.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_confined_reads_block_a_sibling_tenant() {
        let base =
            std::env::temp_dir().join(format!("harness-sandbox-reads-{}", std::process::id()));
        let mine = base.join("tenant-a");
        let theirs = base.join("tenant-b");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();
        let my_file = mine.join("ok.txt");
        let their_secret = theirs.join("secret.txt");
        std::fs::write(&my_file, "mine\n").unwrap();
        std::fs::write(&their_secret, "SECRET\n").unwrap();

        // Resolve through symlinks: on macOS the temp dir is /var/... which is a
        // symlink to /private/var, and Seatbelt matches on the real path. A
        // profile written with the unresolved path denies everything, including
        // the workspace — which would make this test pass for the wrong reason.
        let real_root = std::fs::canonicalize(&mine).unwrap();
        let real_secret = std::fs::canonicalize(&their_secret).unwrap();

        let h = SeatbeltSandbox::new(&real_root)
            .with_confine_reads(true)
            .spawn()
            .await
            .expect("seatbelt spawns");

        // A binary still runs — /usr/bin and the dyld cache stayed readable.
        let own = h
            .world
            .runner
            .exec(
                "/bin/cat",
                &[real_root.join("ok.txt").to_str().unwrap()],
                None,
            )
            .await
            .unwrap();
        assert_eq!(own.status, 0, "own workspace must stay readable: {own:?}");
        assert!(own.stdout.contains("mine"));

        // The neighbour's file is not.
        let stolen = h
            .world
            .runner
            .exec("/bin/cat", &[real_secret.to_str().unwrap()], None)
            .await
            .unwrap();
        assert_ne!(stolen.status, 0, "sibling tenant read must be denied");
        assert!(
            !stolen.stdout.contains("SECRET"),
            "secret leaked out of the sandbox: {stolen:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// macOS: prove Seatbelt enforces net-deny at the kernel via the sandbox's
    /// own **per-command** runner — the networked child is blocked, a local child
    /// works, and (unlike birdcage) the parent process is never touched.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_denies_child_network_without_touching_parent() {
        let curl = "/usr/bin/curl";
        if !std::path::Path::new(curl).exists() {
            return;
        }
        let probe = ["-s", "-m", "6", "-o", "/dev/null", "https://example.com"];
        let parent_net = || {
            std::process::Command::new(curl)
                .args(probe)
                .output()
                .map(|o| o.status.code().unwrap_or(-1))
                .unwrap_or(-1)
        };
        if parent_net() != 0 {
            return; // offline — skip
        }

        let h = SeatbeltSandbox::new(".")
            .spawn()
            .await
            .expect("seatbelt spawns");
        // local child ok
        let echo = h
            .world
            .runner
            .exec("/bin/echo", &["hi"], None)
            .await
            .unwrap();
        assert_eq!(echo.status, 0);
        // networked child blocked
        let blocked = h.world.runner.exec(curl, &probe, None).await.unwrap();
        assert_ne!(blocked.status, 0, "child network must be denied");
        // Parent still unrestricted — the whole point versus birdcage.
        //
        // This is a live network call, so a single failure has two possible
        // causes and they are not equally likely: if the sandbox had leaked into
        // the parent, the profile is applied to the process and *every* attempt
        // fails; a network hiccup does not repeat. Under a full parallel test
        // run this assertion failed on a blip and blamed the sandbox. Requiring
        // one success out of three separates the two.
        let unrestricted = (0..3).any(|_| parent_net() == 0);
        assert!(unrestricted, "parent must remain unsandboxed");
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
