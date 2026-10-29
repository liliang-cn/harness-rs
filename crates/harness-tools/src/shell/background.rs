//! Background jobs — commands that outlive a single tool call.
//!
//! `shell_exec` is spawn → wait → collect: the right shape for `cargo build`,
//! the wrong shape for `go run main.go`. A dev server never exits, so the
//! synchronous tool blocks until the per-call deadline kills the *call* — and
//! since dropping a tokio `Child` does not kill the *process*, the model gets
//! an error while an orphan keeps listening on the port. Three tools fix that:
//!
//! - **`shell_spawn`** — start a process, get a `job_id` back immediately. A
//!   short grace window catches commands that exit at once (those return like
//!   a plain exec, no job created). Output is pumped to a log file on disk;
//!   the model only ever sees bounded slices of it.
//! - **`shell_job_status`** — is it still running, and what has it printed
//!   since the last look (cursor-based, no re-flooding).
//! - **`shell_job_kill`** — SIGTERM to the *process group*, escalating to
//!   SIGKILL. The group matters: `go run` is a compiler process that forks the
//!   real binary — killing only the direct child leaves the server alive.
//!
//! ## Lifecycle — who dies when
//!
//! Every job has a scope, chosen at spawn:
//!
//! - **`run`** (default): dies when this agent run ends. [`JobReaperHook`]
//!   listens for `SessionEnd` (fired on Done / Stuck / BudgetExhausted / Cancelled alike)
//!   and reaps; `kill_on_drop` backstops hard-error paths. "Start server →
//!   curl it → forget to kill" leaks nothing.
//! - **`session`**: survives across turns of the same conversation, so the
//!   next user message can still reach the server started in this one. Dies
//!   on [`JobTable::reap_session`] (host calls it when the conversation is
//!   closed) or the idle TTL ([`JobTable::spawn_ttl_sweeper`]) — a job nobody
//!   has looked at in that long is a leak, not a service.
//! - **`detached`**: the framework starts it and lets go. Never reaped, never
//!   swept, survives host shutdown. Hosts should gate this behind the same
//!   approval as any irreversible action.
//!
//! All jobs run in their own process group (`setpgid`); every kill is a
//! group kill. Ownership is stamped from `World::session` at spawn: a job is
//! only visible to the actor that started it.
//!
//! ## Telling the host — events instead of polling
//!
//! A job is *watched*, not polled: once it outlives the grace window a
//! per-job task awaits the child's exit directly. That task is the only owner
//! of the `Child`, records the exit status in the table (so `status` / `list`
//! see it at once) and, if the host called [`JobTable::subscribe`], sends a
//! [`JobEvent`]:
//!
//! - `Exited` — the process ended (including via `shell_job_kill`).
//! - `Overdue` — the job is still running `expect_secs` after it started
//!   (default 600), and again every `expect_secs` after that. The model sets
//!   `expect_secs` when it spawns, so "CI should take ~20 minutes" becomes a
//!   deadline the host is woken at, instead of a promise nobody keeps.
//!
//! Jobs reaped on purpose (run end, session close, shutdown) are silent: the
//! host asked for that. With no subscriber nothing is queued.
//!
//! The table is host-owned and shared: hand the same `Arc<JobTable>` to the
//! three tools, the reaper hook, and (in a server) your shutdown path — which
//! should call [`JobTable::kill_all_owned`].

use async_trait::async_trait;
use harness_core::Event;
use harness_core::{Hook, HookOutcome, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc, watch};

/// How long `shell_spawn` waits for the command to just finish. Anything that
/// exits inside this window is an ordinary exec, not a job.
const GRACE_MS: u64 = 2000;
/// Max bytes of early output returned by `shell_spawn`.
const SPAWN_PREVIEW_BYTES: usize = 4096;
/// Max new-output bytes per `shell_job_status` call.
const STATUS_SLICE_BYTES: usize = 8192;
/// SIGTERM → SIGKILL escalation delay.
const TERM_GRACE_MS: u64 = 3000;
/// Ceiling on `shell_job_status`'s in-call wait. Kept well under the loop's
/// default 120s per-call tool deadline, which would otherwise turn a
/// legitimate wait into a tool error.
const MAX_WAIT_MS: u64 = 60_000;
/// How often an in-call wait re-reads the job.
const WAIT_TICK: Duration = Duration::from_millis(250);
/// How long a job may run before the host is told it is overdue, when the
/// spawner gave no estimate.
const DEFAULT_EXPECT_SECS: u64 = 600;
/// How long the watcher waits for the output pumps to hit EOF after the child
/// exits. A grandchild that inherited the pipes can hold them open forever.
const PUMP_FLUSH_MS: u64 = 500;
/// The tail carried by a [`JobEvent`]: at most this many lines...
const TAIL_LINES: usize = 8;
/// ...of at most this many bytes in all.
const TAIL_BYTES: usize = 2048;

/// Which cleanup tier a job belongs to. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobScope {
    Run,
    Session,
    Detached,
}

/// Everything `JobTable::spawn` needs. Hosts that gate spawning behind an
/// approval flow build one of these and call the table directly.
#[derive(Debug, Clone, Deserialize)]
pub struct SpawnRequest {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Relative to the workspace root.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_scope")]
    pub scope: JobScope,
    /// How long the spawner expects this to run, in seconds. When it is still
    /// running after that, the host gets a [`JobEvent::Overdue`] (and another
    /// every interval after). Default 600.
    #[serde(default)]
    pub expect_secs: Option<u64>,
}

fn default_scope() -> JobScope {
    JobScope::Run
}

/// What a spawn produced: a finished command, or a live job.
pub enum Spawned {
    /// Exited within the grace window — no job entry was created.
    Exited { status: i32, output: String },
    /// Still running; poll with `shell_job_status`, stop with `shell_job_kill`.
    Running { id: u64, pid: u32, preview: String },
}

/// What the host hears about a background job. Sent only for jobs that
/// outlived the grace window and were not reaped on purpose.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum JobEvent {
    /// The process ended; `exit_status` is -1 when it was killed by a signal.
    Exited {
        id: u64,
        program: String,
        exit_status: i32,
        running_for: Duration,
        /// Last few lines of the log, bounded.
        tail: String,
    },
    /// Still running past its `expect_secs`. Repeats every interval.
    Overdue {
        id: u64,
        program: String,
        running_for: Duration,
        tail: String,
    },
}

struct Job {
    program: String,
    args: Vec<String>,
    /// The `Child` itself lives in the job's watcher task; these are the
    /// handles to it: exit status (`None` until it ends) and a hard-kill nudge.
    exit_rx: watch::Receiver<Option<i32>>,
    hard_kill: Arc<Notify>,
    expect_secs: u64,
    overdue: u32,
    pid: u32,
    scope: JobScope,
    /// Ownership stamps from `World::session` at spawn time.
    actor: Option<String>,
    session: Option<String>,
    request: Option<String>,
    log_path: PathBuf,
    read_offset: u64,
    started_ms: i64,
    /// Monotonic twin of `started_ms`, for "how long has this been running"
    /// without trusting a wall clock that can jump.
    started_at: Instant,
    last_touch: Instant,
    exit: Option<i32>,
}

fn world_actor(world: &World) -> Option<&str> {
    world
        .session
        .as_ref()
        .filter(|s| !s.actor.is_empty())
        .map(|s| s.actor.as_str())
}

impl Job {
    fn visible_to(&self, actor: Option<&str>) -> bool {
        self.actor.as_deref() == actor
    }

    fn state(&self) -> &'static str {
        if self.exit.is_some() {
            "exited"
        } else {
            "running"
        }
    }
}

/// The shared registry of live jobs. One per host process; see module docs.
pub struct JobTable {
    // std Mutex: never held across an await. Arc'd because each job's watcher
    // task records its exit here.
    jobs: Arc<Mutex<HashMap<u64, Job>>>,
    events: Arc<Mutex<Option<mpsc::UnboundedSender<JobEvent>>>>,
    next: AtomicU64,
}

impl Default for JobTable {
    fn default() -> Self {
        Self::new()
    }
}

impl JobTable {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            events: Arc::new(Mutex::new(None)),
            next: AtomicU64::new(1),
        }
    }

    /// Receive [`JobEvent`]s for every background job of this table. One
    /// subscriber at a time: a later call replaces the earlier one. Drop the
    /// receiver and events are discarded again.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<JobEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.events.lock().unwrap() = Some(tx);
        rx
    }

    /// Start a process in its own process group, pumping output to
    /// `.harness/jobs/job-<id>.log` under the workspace root.
    pub async fn spawn(&self, req: &SpawnRequest, world: &mut World) -> Result<Spawned, ToolError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let dir = world.repo.root.join(".harness").join("jobs");
        std::fs::create_dir_all(&dir)
            .map_err(|e| ToolError::Exec(format!("create {}: {e}", dir.display())))?;
        let log_path = dir.join(format!("job-{id}.log"));
        let log = std::fs::File::create(&log_path)
            .map_err(|e| ToolError::Exec(format!("create {}: {e}", log_path.display())))?;

        let cwd = match &req.cwd {
            Some(c) => world.repo.root.join(c),
            None => world.repo.root.clone(),
        };
        let mut cmd = tokio::process::Command::new(&req.program);
        cmd.args(&req.args)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(req.scope != JobScope::Detached);
        #[cfg(unix)]
        cmd.process_group(0); // its own group: group-kill reaches grandchildren

        let started_at = Instant::now(); // process start, not "grace window over"
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Exec(format!("spawn `{}`: {e}", req.program)))?;
        let pid = child.id().unwrap_or(0);

        // Pump both streams into the one log file, interleaved by chunk.
        let mut pumps = Vec::new();
        if let Some(out) = child.stdout.take() {
            let f = log
                .try_clone()
                .map_err(|e| ToolError::Exec(e.to_string()))?;
            pumps.push(tokio::spawn(pump(out, f)));
        }
        if let Some(err) = child.stderr.take() {
            pumps.push(tokio::spawn(pump(err, log)));
        }

        // Grace window: a command that exits now was never a background job.
        match tokio::time::timeout(Duration::from_millis(GRACE_MS), child.wait()).await {
            Ok(status) => {
                let status = status
                    .map_err(|e| ToolError::Exec(e.to_string()))?
                    .code()
                    .unwrap_or(-1);
                for p in pumps {
                    let _ = p.await; // EOF is guaranteed; flush before reading
                }
                let output = read_slice(&log_path, 0, SPAWN_PREVIEW_BYTES).0;
                let _ = std::fs::remove_file(&log_path);
                Ok(Spawned::Exited { status, output })
            }
            Err(_) => {
                let preview = read_slice(&log_path, 0, SPAWN_PREVIEW_BYTES).0;
                let log_path_for_watch = log_path.clone();
                let (exit_tx, exit_rx) = watch::channel(None);
                let hard_kill = Arc::new(Notify::new());
                let expect_secs = req.expect_secs.unwrap_or(DEFAULT_EXPECT_SECS).max(1);
                let job = Job {
                    program: req.program.clone(),
                    args: req.args.clone(),
                    exit_rx,
                    hard_kill: hard_kill.clone(),
                    expect_secs,
                    overdue: 0,
                    pid,
                    scope: req.scope,
                    actor: world
                        .session
                        .as_ref()
                        .filter(|s| !s.actor.is_empty())
                        .map(|s| s.actor.clone()),
                    session: world.session.as_ref().map(|s| s.id.clone()),
                    request: world.session.as_ref().map(|s| s.request.clone()),
                    log_path,
                    read_offset: preview.len() as u64,
                    started_ms: world.clock.now_ms(),
                    started_at,
                    last_touch: Instant::now(),
                    exit: None,
                };
                self.jobs.lock().unwrap().insert(id, job);
                tokio::spawn(
                    Watcher {
                        id,
                        program: req.program.clone(),
                        log_path: log_path_for_watch,
                        started_at,
                        interval: Duration::from_secs(expect_secs),
                        jobs: self.jobs.clone(),
                        events: self.events.clone(),
                        exit_tx,
                        hard_kill,
                    }
                    .run(child, pumps),
                );
                Ok(Spawned::Running { id, pid, preview })
            }
        }
    }

    /// One job's state + everything it printed since the last look.
    pub fn status(&self, id: u64, world: &World) -> Option<Value> {
        self.status_for(id, world_actor(world))
    }

    /// Same, keyed by actor directly — for HTTP surfaces with no `World`.
    pub fn status_for(&self, id: u64, actor: Option<&str>) -> Option<Value> {
        let (path, offset, base) = {
            let mut jobs = self.jobs.lock().unwrap();
            let job = jobs.get_mut(&id).filter(|j| j.visible_to(actor))?;
            job.last_touch = Instant::now();
            (
                job.log_path.clone(),
                job.read_offset,
                json!({
                    "id": id, "program": job.program, "state": job.state(),
                    "exit_status": job.exit, "pid": job.pid,
                    // How long it has been alive. The model needs this to
                    // decide whether to keep waiting, and a build that has
                    // been silent for eight minutes reads very differently
                    // from one that started two seconds ago.
                    "running_for_ms": job.started_at.elapsed().as_millis() as u64,
                }),
            )
        };
        let (new_output, new_offset, skipped) = read_slice(&path, offset, STATUS_SLICE_BYTES);
        if let Some(job) = self.jobs.lock().unwrap().get_mut(&id) {
            job.read_offset = new_offset;
        }
        let mut v = base;
        v["new_output"] = json!(new_output);
        if skipped > 0 {
            v["note"] = json!(format!(
                "{skipped} bytes of output skipped (only the most recent {STATUS_SLICE_BYTES} \
                 bytes are shown; full log: {})",
                path.display()
            ));
        }
        Some(v)
    }

    /// All jobs visible to this caller.
    pub fn list(&self, world: &World) -> Vec<Value> {
        self.list_for(world_actor(world))
    }

    /// Same, keyed by actor directly — for HTTP surfaces with no `World`.
    pub fn list_for(&self, actor: Option<&str>) -> Vec<Value> {
        let jobs = self.jobs.lock().unwrap();
        let mut rows: Vec<(u64, Value)> = jobs
            .iter()
            .filter(|(_, j)| j.visible_to(actor))
            .map(|(id, j)| {
                (
                    *id,
                    json!({
                        "id": id, "program": j.program, "args": j.args,
                        "state": j.state(), "exit_status": j.exit,
                        "scope": format!("{:?}", j.scope).to_lowercase(),
                        "started_ms": j.started_ms,
                        "running_for_ms": j.started_at.elapsed().as_millis() as u64,
                        "expect_secs": j.expect_secs,
                        "overdue_count": j.overdue,
                    }),
                )
            })
            .collect();
        rows.sort_by_key(|(id, _)| *id);
        rows.into_iter().map(|(_, v)| v).collect()
    }

    /// Group-SIGTERM, escalate to group-SIGKILL, wait for the exit. The entry
    /// stays (state `exited`) so a final `shell_job_status` can read the tail.
    pub async fn kill(&self, id: u64, world: &World) -> Option<Value> {
        self.kill_for(id, world_actor(world)).await
    }

    /// Same, keyed by actor directly — for HTTP surfaces with no `World`.
    pub async fn kill_for(&self, id: u64, actor: Option<&str>) -> Option<Value> {
        let (rx, hard_kill, pid) = {
            let jobs = self.jobs.lock().unwrap();
            let job = jobs.get(&id).filter(|j| j.visible_to(actor))?;
            if job.exit.is_some() {
                return Some(json!({ "id": id, "state": "exited", "exit_status": job.exit }));
            }
            (job.exit_rx.clone(), job.hard_kill.clone(), job.pid)
        };
        // The watcher owns the child and records the exit (and tells the
        // host); this only signals and waits for it to have happened.
        let exit = terminate(rx, hard_kill, pid).await;
        Some(json!({ "id": id, "state": "killed", "exit_status": exit }))
    }

    /// End-of-run cleanup: every `run`-scoped job of this request dies.
    /// Called by [`JobReaperHook`] on `SessionEnd`; hosts may also call it
    /// directly after `run()` returns on an error path.
    pub fn reap_run(&self, request: Option<&str>) {
        self.reap_where(|j| j.scope == JobScope::Run && j.request.as_deref() == request);
    }

    /// Conversation closed: every `session`-scoped job of that conversation
    /// dies. (Run-scoped ones are already gone.)
    pub fn reap_session(&self, session_id: &str) {
        self.reap_where(|j| {
            j.scope == JobScope::Session && j.session.as_deref() == Some(session_id)
        });
    }

    /// Host is shutting down: everything non-detached dies.
    pub fn kill_all_owned(&self) {
        self.reap_where(|j| j.scope != JobScope::Detached);
    }

    /// Kill `session`-scoped jobs idle longer than `ttl`, then prune exited
    /// entries older than it. Call once from a host task, keep the handle.
    pub fn spawn_ttl_sweeper(
        self: &std::sync::Arc<Self>,
        ttl: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let table = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                table.reap_where(|j| j.scope == JobScope::Session && j.last_touch.elapsed() > ttl);
                table
                    .jobs
                    .lock()
                    .unwrap()
                    .retain(|_, j| j.exit.is_none() || j.last_touch.elapsed() <= ttl);
            }
        })
    }

    /// Sync (callable from a hook): signal matching jobs and remove their
    /// entries; escalation runs on the runtime when one is available.
    fn reap_where(&self, pred: impl Fn(&Job) -> bool) {
        let doomed: Vec<Job> = {
            let mut jobs = self.jobs.lock().unwrap();
            let ids: Vec<u64> = jobs
                .iter()
                .filter(|(_, j)| j.exit.is_none() && pred(j))
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter().filter_map(|id| jobs.remove(&id)).collect()
        };
        for job in doomed {
            signal_group(job.pid, Sig::Term);
            let (rx, hard_kill, pid) = (job.exit_rx, job.hard_kill, job.pid);
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn(async move {
                        terminate(rx, hard_kill, pid).await;
                    });
                }
                // No runtime (host is tearing down): no grace, just end it.
                // The watcher task dies with the runtime and `kill_on_drop`
                // finishes the direct child.
                Err(_) => signal_group(pid, Sig::Kill),
            }
        }
    }
}

/// SIGTERM the group, give it `TERM_GRACE_MS`, then SIGKILL the group. Waits
/// for the watcher to report the exit and returns the direct child's status
/// (-1 when unknowable).
async fn terminate(
    mut exit: watch::Receiver<Option<i32>>,
    hard_kill: Arc<Notify>,
    pid: u32,
) -> i32 {
    signal_group(pid, Sig::Term);
    if let Some(code) = wait_exit(&mut exit, TERM_GRACE_MS).await {
        return code;
    }
    signal_group(pid, Sig::Kill);
    hard_kill.notify_one(); // non-unix fallback: the watcher `start_kill`s its child
    wait_exit(&mut exit, TERM_GRACE_MS).await.unwrap_or(-1)
}

async fn wait_exit(exit: &mut watch::Receiver<Option<i32>>, ms: u64) -> Option<i32> {
    tokio::time::timeout(Duration::from_millis(ms), exit.wait_for(|c| c.is_some()))
        .await
        .ok()?
        .ok()
        .and_then(|c| *c)
}

/// One per background job: owns the `Child`, so it can await the exit itself
/// and race it against the job's deadline. The only place that learns a job
/// ended — everything else (status, kill, the host) hears it from here.
struct Watcher {
    id: u64,
    program: String,
    log_path: PathBuf,
    started_at: Instant,
    interval: Duration,
    jobs: Arc<Mutex<HashMap<u64, Job>>>,
    events: Arc<Mutex<Option<mpsc::UnboundedSender<JobEvent>>>>,
    exit_tx: watch::Sender<Option<i32>>,
    hard_kill: Arc<Notify>,
}

impl Watcher {
    async fn run(self, mut child: tokio::process::Child, pumps: Vec<tokio::task::JoinHandle<()>>) {
        // Measured from process start, so a deadline shorter than the grace
        // window fires the moment the watch begins.
        let mut next = tokio::time::Instant::from_std(self.started_at + self.interval);
        let mut deadline = Box::pin(tokio::time::sleep_until(next));
        let status = loop {
            tokio::select! {
                st = child.wait() => break st.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1),
                _ = &mut deadline => {
                    next += self.interval;
                    deadline.as_mut().reset(next);
                    // A reaped job (entry gone) is silent: the host ended it.
                    let known = self.jobs.lock().unwrap().get_mut(&self.id).map(|j| j.overdue += 1);
                    if known.is_some() {
                        self.emit(JobEvent::Overdue {
                            id: self.id,
                            program: self.program.clone(),
                            running_for: self.started_at.elapsed(),
                            tail: self.tail(),
                        });
                    }
                }
                _ = self.hard_kill.notified() => {
                    let _ = child.start_kill();
                }
            }
        };
        // Flush what the job printed before anyone reads the tail, but don't
        // wait on a grandchild that kept the pipes open.
        for p in pumps {
            let _ = tokio::time::timeout(Duration::from_millis(PUMP_FLUSH_MS), p).await;
        }
        let known = {
            let mut jobs = self.jobs.lock().unwrap();
            jobs.get_mut(&self.id).map(|j| j.exit = Some(status))
        };
        let _ = self.exit_tx.send(Some(status));
        if known.is_some() {
            self.emit(JobEvent::Exited {
                id: self.id,
                program: self.program.clone(),
                exit_status: status,
                running_for: self.started_at.elapsed(),
                tail: self.tail(),
            });
        }
    }

    fn tail(&self) -> String {
        tail_of(&read_slice(&self.log_path, 0, TAIL_BYTES).0, TAIL_LINES)
    }

    fn emit(&self, ev: JobEvent) {
        if let Some(tx) = self.events.lock().unwrap().as_ref() {
            let _ = tx.send(ev); // receiver gone = nobody is listening
        }
    }
}

/// The last `lines` non-blank lines of `text`.
fn tail_of(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

enum Sig {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: Sig) {
    if pid == 0 {
        return;
    }
    let sig = match sig {
        Sig::Term => libc::SIGTERM,
        Sig::Kill => libc::SIGKILL,
    };
    // Negative pid = the whole process group (pgid == pid via process_group(0)).
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _sig: Sig) {
    // No process groups: `kill_on_drop` / `start_kill` handle the direct child.
}

async fn pump(mut src: impl tokio::io::AsyncRead + Unpin, dst: std::fs::File) {
    let mut dst = tokio::fs::File::from_std(dst);
    let _ = tokio::io::copy(&mut src, &mut dst).await;
}

/// Read up to `cap` bytes starting at `offset`; when more than `cap` is new,
/// return the most recent `cap` bytes and how many were skipped.
fn read_slice(path: &std::path::Path, offset: u64, cap: usize) -> (String, u64, u64) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (String::new(), offset, 0);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= offset {
        return (String::new(), offset, 0);
    }
    let new = len - offset;
    let (start, skipped) = if new > cap as u64 {
        (len - cap as u64, new - cap as u64)
    } else {
        (offset, 0)
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (String::new(), offset, 0);
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if f.read_to_end(&mut buf).is_err() {
        return (String::new(), offset, 0);
    }
    (String::from_utf8_lossy(&buf).into_owned(), len, skipped)
}

// ---------- the tools ----------

/// Tool: start a background job. See the module docs for lifecycle.
pub struct ShellSpawn {
    table: Arc<JobTable>,
    default_scope: JobScope,
    schema: ToolSchema,
}

impl JobScope {
    fn as_str(self) -> &'static str {
        match self {
            JobScope::Run => "run",
            JobScope::Session => "session",
            JobScope::Detached => "detached",
        }
    }
}

fn spawn_schema(default_scope: JobScope) -> ToolSchema {
    let scopes = [
        ("run", "killed automatically when this task ends"),
        (
            "session",
            "survives into later turns of this conversation, reclaimed on idle timeout",
        ),
        (
            "detached",
            "outlives everything; only when the user explicitly asks for that",
        ),
    ];
    let scope_doc = scopes
        .iter()
        .map(|(name, what)| {
            if *name == default_scope.as_str() {
                format!("'{name}' (default — {what})")
            } else {
                format!("'{name}' ({what})")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    ToolSchema {
        name: "shell_spawn".into(),
        description: format!(
            "Start a long-running command in the background (dev server, watcher, CI wait, \
             anything that does not exit on its own) and return a job_id immediately. Commands \
             that finish within ~2s are returned directly like shell_exec. Poll with \
             shell_job_status, stop with shell_job_kill. The host tells you when a job exits, \
             and again if it is still running past expect_secs — so you do not need to poll or \
             sleep: say what you started and stop. Always set expect_secs to your honest \
             estimate of how long the command takes (default 600), and tell the user that ETA. \
             scope: {scope_doc}."
        ),
        input: json!({
            "type": "object",
            "properties": {
                "program": {"type": "string"},
                "args": {"type": "array", "items": {"type": "string"}},
                "cwd": {"type": "string", "description": "Relative to workspace root"},
                "scope": {
                    "type": "string", "enum": ["run", "session", "detached"],
                    "default": default_scope.as_str()
                },
                "expect_secs": {
                    "type": "integer", "minimum": 1,
                    "description": "Expected run time in seconds. You are notified if the \
                                    job is still running after this (and again each interval). \
                                    Default 600."
                }
            },
            "required": ["program"]
        }),
    }
}

impl ShellSpawn {
    pub fn new(table: Arc<JobTable>) -> Self {
        Self {
            table,
            default_scope: JobScope::Run,
            schema: spawn_schema(JobScope::Run),
        }
    }

    /// Scope for jobs whose call does not name one. `run` unless the host
    /// outlives a single agent run — an interactive REPL, where a job started
    /// this turn must still be there to report on the next.
    pub fn with_default_scope(mut self, scope: JobScope) -> Self {
        self.default_scope = scope;
        self.schema = spawn_schema(scope);
        self
    }
}

#[async_trait]
impl Tool for ShellSpawn {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Destructive
    }
    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        let mut args = args;
        if let Some(o) = args.as_object_mut() {
            o.entry("scope")
                .or_insert(json!(self.default_scope.as_str()));
        }
        let req: SpawnRequest =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "shell_spawn".into(),
                reason: e.to_string(),
            })?;
        Ok(spawn_to_result(self.table.spawn(&req, world).await?))
    }
}

/// Shape a [`Spawned`] into the tool result both `ShellSpawn` and host
/// wrappers (approval-gated spawners) hand to the model.
pub fn spawn_to_result(spawned: Spawned) -> ToolResult {
    match spawned {
        Spawned::Exited { status, output } => ToolResult {
            ok: status == 0,
            content: json!({ "exited": true, "status": status, "output": output }),
            trace: None,
        },
        Spawned::Running { id, pid, preview } => ToolResult {
            ok: true,
            content: json!({
                "job_id": id, "pid": pid, "state": "running", "output_so_far": preview,
                "next": "poll shell_job_status; kill with shell_job_kill when done"
            }),
            trace: Some(format!("job {id} (pid {pid}) running")),
        },
    }
}

/// Tool: poll one job (or list them all).
pub struct ShellJobStatus {
    table: Arc<JobTable>,
    schema: ToolSchema,
}

impl ShellJobStatus {
    pub fn new(table: Arc<JobTable>) -> Self {
        Self {
            table,
            schema: ToolSchema {
                name: "shell_job_status".into(),
                description: "Check a background job started by shell_spawn: running or exited, \
                              how long it has been going, and everything it printed since you \
                              last checked. Without an id, lists all your jobs. Pass wait_ms to \
                              WAIT inside this one call until there is new output or the job \
                              exits — always prefer that over calling this repeatedly in a \
                              tight loop, which burns your step budget on nothing."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer"},
                        "wait_ms": {
                            "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS,
                            "description": "Block up to this long for new output or exit."
                        }
                    }
                }),
            },
        }
    }
}

impl ShellJobStatus {
    /// One status read, or — with a `wait` budget — the first read that shows
    /// something new: output, or the job exiting. Polls internally at a fixed
    /// tick so a model waiting ten minutes on a build spends one step, not
    /// hundreds. Accumulates output across the internal polls, since each read
    /// advances the job's cursor and dropping it would lose those lines.
    async fn wait_for_change(&self, id: u64, wait_ms: u64, world: &World) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut acc = String::new();
        loop {
            let mut v = self.table.status_for(id, world_actor(world))?;
            if let Some(s) = v["new_output"].as_str() {
                acc.push_str(s);
            }
            let exited = v["state"] == "exited";
            if exited || !acc.is_empty() || Instant::now() >= deadline {
                v["new_output"] = json!(acc);
                if !exited && wait_ms > 0 && v["new_output"] == json!("") {
                    v["waited_ms"] = json!(wait_ms);
                }
                return Some(v);
            }
            tokio::time::sleep(WAIT_TICK).await;
        }
    }
}

#[async_trait]
impl Tool for ShellJobStatus {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        let wait = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(MAX_WAIT_MS);
        match args.get("id").and_then(Value::as_u64) {
            Some(id) => match self.wait_for_change(id, wait, world).await {
                Some(v) => Ok(ToolResult {
                    ok: true,
                    content: v,
                    trace: None,
                }),
                None => Ok(ToolResult {
                    ok: false,
                    content: json!({ "error": format!("no job {id}") }),
                    trace: None,
                }),
            },
            None => Ok(ToolResult {
                ok: true,
                content: json!({ "jobs": self.table.list(world) }),
                trace: None,
            }),
        }
    }
}

/// Tool: stop one job (group SIGTERM → SIGKILL).
pub struct ShellJobKill {
    table: Arc<JobTable>,
    schema: ToolSchema,
}

impl ShellJobKill {
    pub fn new(table: Arc<JobTable>) -> Self {
        Self {
            table,
            schema: ToolSchema {
                name: "shell_job_kill".into(),
                description: "Stop a background job started by shell_spawn. Kills the whole \
                              process group (SIGTERM, then SIGKILL after a few seconds), so \
                              servers started via wrappers like `go run` really stop."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": { "id": {"type": "integer"} },
                    "required": ["id"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for ShellJobKill {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Idempotent
    }
    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        let id = args
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "shell_job_kill".into(),
                reason: "id required".into(),
            })?;
        match self.table.kill(id, world).await {
            Some(v) => Ok(ToolResult {
                ok: true,
                content: v,
                trace: None,
            }),
            None => Ok(ToolResult {
                ok: false,
                content: json!({ "error": format!("no job {id}") }),
                trace: None,
            }),
        }
    }
}

/// Hook: when the run ends (any normal outcome — `SessionEnd` fires on Done,
/// Stuck, BudgetExhausted and Cancelled alike), reap this run's `run`-scoped
/// jobs.
pub struct JobReaperHook {
    table: Arc<JobTable>,
}

impl JobReaperHook {
    pub fn new(table: Arc<JobTable>) -> Self {
        Self { table }
    }
}

impl Hook for JobReaperHook {
    fn name(&self) -> &str {
        "job-reaper"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::SessionEnd)
    }
    fn fire(&self, _ev: &Event<'_>, world: &mut World) -> HookOutcome {
        self.table
            .reap_run(world.session.as_ref().map(|s| s.request.as_str()));
        HookOutcome::Allow
    }
}
