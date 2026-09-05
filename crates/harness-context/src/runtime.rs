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
/// does not. The child is spawned as a group leader (`process_group(0)`), so
/// signalling `-pid` reaches everything it started. Disarm on the normal exit
/// path: a child that finished on its own may have deliberately left something
/// behind, and the runner has no business killing that.
#[cfg(unix)]
pub struct GroupKill {
    pid: Option<i32>,
}

#[cfg(unix)]
impl GroupKill {
    /// Arm for `child`, which must have been spawned with `process_group(0)`.
    pub fn arm(child: &tokio::process::Child) -> Self {
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
        // What `Command::output` sets up implicitly, made explicit because the
        // child is spawned by hand below to get its pid before it is awaited.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Dropping this future — a cancelled run, a tool deadline — must
            // stop the child, not orphan it into the background.
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn()?;
        #[cfg(unix)]
        let guard = GroupKill::arm(&child);
        let out = child.wait_with_output().await?;
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
        let grandchild: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
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
}
