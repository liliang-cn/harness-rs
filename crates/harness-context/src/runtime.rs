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

/// Kills a child's whole process group when dropped, unless disarmed.
///
/// `kill_on_drop` reaches only the direct child. A `cargo test` or an external
/// agent starts compilers and test runners of its own, and those are what keep
/// running when a run is cancelled mid-tool: the direct child dies, its group
/// does not. [`GroupKill::spawn`] starts the child as the leader of its own
/// group, so signalling `-pid` reaches everything it started. Disarm on the
/// normal exit path: a child that finished on its own may have deliberately
/// left something behind, and the runner has no business killing that.
///
/// Reaches only processes on this host. A runner that fronts `docker exec`
/// kills the client; what runs inside the container is not in the group.
#[cfg(unix)]
#[derive(Debug)]
#[must_use = "dropping the guard immediately kills the group it was just armed for"]
pub struct GroupKill {
    pid: Option<i32>,
}

#[cfg(unix)]
impl GroupKill {
    /// Spawn `cmd` as the leader of a new process group, with `kill_on_drop`
    /// set, and return it together with an armed guard. This is the only way
    /// to obtain a guard, so the setup it depends on cannot be forgotten.
    pub fn spawn(
        cmd: &mut tokio::process::Command,
    ) -> std::io::Result<(tokio::process::Child, GroupKill)> {
        cmd.kill_on_drop(true).process_group(0);
        let child = cmd.spawn()?;
        let guard = GroupKill::arm(&child);
        Ok((child, guard))
    }

    fn arm(child: &tokio::process::Child) -> Self {
        Self {
            pid: child.id().map(|p| p as i32),
        }
    }

    /// The child exited on its own; leave whatever it left behind alone.
    pub fn disarm(mut self) {
        self.pid = None;
    }
}

#[cfg(unix)]
impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            // SAFETY: a plain signal to a process group this runner created.
            // ESRCH (already gone) is the only expected failure and is fine.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
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
        // stdin is closed, not inherited. tokio's `Command::output` — unlike
        // std's — leaves stdin inheriting the parent's, so before this a tool's
        // child could read the user's keystrokes out from under a REPL, or hang
        // forever on `git commit` waiting for an editor that never comes.
        // Nothing a tool runs should be reading a terminal.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Dropping this future — a cancelled run, a tool deadline — must stop
        // the child and everything it started, not orphan them.
        #[cfg(unix)]
        let (child, guard) = GroupKill::spawn(&mut cmd)?;
        #[cfg(not(unix))]
        let child = {
            cmd.kill_on_drop(true);
            cmd.spawn()?
        };

        let out = child.wait_with_output().await?;
        // Still armed across the `?` above, on purpose: a wait that failed is
        // not the child exiting on its own, and the tree it started should not
        // outlive the error. (Pid reuse in that window is theoretical — the
        // child is unreaped, so its pid is still held.)
        #[cfg(unix)]
        guard.disarm();

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
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryKv {
    fn default() -> Self {
        Self::new()
    }
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

#[cfg(all(test, unix))]
mod process_group_on_drop {
    use super::TokioRunner;
    use harness_core::ProcessRunner;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Signal 0 probes without delivering; a pid that is dead and reaped fails it.
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// `echo $! > file` writes the digits and a newline in one `write(2)`.
    /// Insist on the newline so a torn read cannot parse a prefix of the pid
    /// as a different, real process. Bounded, so a `sh` that never writes
    /// fails the test instead of hanging CI.
    async fn read_pidfile(path: &std::path::Path) -> i32 {
        for _ in 0..250 {
            if let Some(p) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.strip_suffix('\n').map(str::to_owned))
                .and_then(|s| s.parse().ok())
            {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("sh never wrote its grandchild's pid to {}", path.display());
    }

    /// `sh` is the direct child; the backgrounded `sleep` is its grandchild,
    /// and `sh` writes the grandchild's pid to a file before waiting on it.
    /// Dropping the exec future has to kill the grandchild as well —
    /// `kill_on_drop` alone reaches `sh`, and the sleep would run on for
    /// thirty seconds after the run reported itself cancelled.
    #[tokio::test]
    async fn dropping_an_exec_kills_the_whole_process_group() {
        let pidfile = std::env::temp_dir().join(format!(
            "harness-group-kill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let runner = Arc::new(TokioRunner);
        let handle = tokio::spawn({
            let runner = runner.clone();
            async move { runner.exec("sh", &["-c", script.as_str()], None).await }
        });

        // Wait until sh has started the grandchild and recorded its pid.
        let grandchild = read_pidfile(&pidfile).await;
        assert!(
            alive(grandchild),
            "grandchild should be running before the drop"
        );

        // Dropping the future is exactly what a cancelled run does.
        handle.abort();
        let _ = handle.await;

        // SIGKILL delivery and reaping are asynchronous; allow a moment.
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(grandchild) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_file(&pidfile);
        assert!(
            !alive(grandchild),
            "the grandchild survived the drop: the process group was not killed"
        );
    }

    /// The other half of the contract: a child that exits on its own may have
    /// deliberately left something behind — an agent's `nohup server &` — and
    /// the runner leaves it alone. Without `disarm`, the guard would kill it.
    #[tokio::test]
    async fn a_child_that_exits_normally_keeps_its_detached_grandchild() {
        let pidfile = std::env::temp_dir().join(format!(
            "harness-group-keep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // No `wait`: sh records the grandchild and exits. The grandchild's
        // stdio is redirected so it does not hold the runner's pipes open —
        // otherwise `wait_with_output` would not return until it exited.
        let script = format!(
            "sleep 5 >/dev/null 2>&1 & echo $! > '{}'",
            pidfile.display()
        );
        let out = TokioRunner
            .exec("sh", &["-c", script.as_str()], None)
            .await
            .unwrap();
        assert_eq!(out.status, 0, "sh itself should have exited cleanly");

        let grandchild = read_pidfile(&pidfile).await;
        let survived = alive(grandchild);
        // Tidy up what the test deliberately left behind, whatever the verdict.
        unsafe {
            libc::kill(grandchild, libc::SIGKILL);
        }
        let _ = std::fs::remove_file(&pidfile);
        assert!(
            survived,
            "a detached grandchild must survive its parent's normal exit; the guard was not disarmed"
        );
    }
}
