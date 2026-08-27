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
//!   listens for `SessionEnd` (fired on Done / Stuck / BudgetExhausted alike)
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
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long `shell_spawn` waits for the command to just finish. Anything that
/// exits inside this window is an ordinary exec, not a job.
const GRACE_MS: u64 = 2000;
/// Max bytes of early output returned by `shell_spawn`.
const SPAWN_PREVIEW_BYTES: usize = 4096;
/// Max new-output bytes per `shell_job_status` call.
const STATUS_SLICE_BYTES: usize = 8192;
/// SIGTERM → SIGKILL escalation delay.
const TERM_GRACE_MS: u64 = 3000;

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

struct Job {
    program: String,
    args: Vec<String>,
    child: Option<tokio::process::Child>,
    pid: u32,
    scope: JobScope,
    /// Ownership stamps from `World::session` at spawn time.
    actor: Option<String>,
    session: Option<String>,
    request: Option<String>,
    log_path: PathBuf,
    read_offset: u64,
    started_ms: i64,
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
    jobs: Mutex<HashMap<u64, Job>>, // std Mutex: never held across an await
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
            jobs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
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
                let job = Job {
                    program: req.program.clone(),
                    args: req.args.clone(),
                    child: Some(child),
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
                    last_touch: Instant::now(),
                    exit: None,
                };
                self.jobs.lock().unwrap().insert(id, job);
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
            if job.exit.is_none()
                && let Some(child) = job.child.as_mut()
                && let Ok(Some(st)) = child.try_wait()
            {
                job.exit = Some(st.code().unwrap_or(-1));
                job.child = None; // reaped; nothing left to kill
            }
            (
                job.log_path.clone(),
                job.read_offset,
                json!({
                    "id": id, "program": job.program, "state": job.state(),
                    "exit_status": job.exit, "pid": job.pid,
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
        let mut jobs = self.jobs.lock().unwrap();
        let mut rows: Vec<(u64, Value)> = jobs
            .iter_mut()
            .filter(|(_, j)| j.visible_to(actor))
            .map(|(id, j)| {
                if j.exit.is_none()
                    && let Some(child) = j.child.as_mut()
                    && let Ok(Some(st)) = child.try_wait()
                {
                    j.exit = Some(st.code().unwrap_or(-1));
                    j.child = None;
                }
                (
                    *id,
                    json!({
                        "id": id, "program": j.program, "args": j.args,
                        "state": j.state(), "exit_status": j.exit,
                        "scope": format!("{:?}", j.scope).to_lowercase(),
                        "started_ms": j.started_ms,
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
        let (child, pid) = {
            let mut jobs = self.jobs.lock().unwrap();
            let job = jobs.get_mut(&id).filter(|j| j.visible_to(actor))?;
            if job.exit.is_some() {
                return Some(json!({ "id": id, "state": "exited", "exit_status": job.exit }));
            }
            (job.child.take(), job.pid)
        };
        let exit = terminate(child, pid).await;
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(&id) {
            job.exit = Some(exit);
        }
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
            let (child, pid) = (job.child, job.pid);
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn(async move {
                        terminate(child, pid).await;
                    });
                }
                // No runtime (host is tearing down): no grace, just end it.
                Err(_) => {
                    signal_group(pid, Sig::Kill);
                    drop(child); // kill_on_drop finishes the direct child
                }
            }
        }
    }
}

/// SIGTERM the group, give it `TERM_GRACE_MS`, then SIGKILL the group.
/// Returns the exit status of the direct child (-1 when unknowable).
async fn terminate(child: Option<tokio::process::Child>, pid: u32) -> i32 {
    signal_group(pid, Sig::Term);
    let Some(mut child) = child else {
        // Nothing to await on: schedule the hard stop and move on.
        tokio::time::sleep(Duration::from_millis(TERM_GRACE_MS)).await;
        signal_group(pid, Sig::Kill);
        return -1;
    };
    match tokio::time::timeout(Duration::from_millis(TERM_GRACE_MS), child.wait()).await {
        Ok(st) => st.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1),
        Err(_) => {
            signal_group(pid, Sig::Kill);
            let _ = child.start_kill(); // non-unix fallback; no-op if already dead
            child
                .wait()
                .await
                .map(|s| s.code().unwrap_or(-1))
                .unwrap_or(-1)
        }
    }
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

use std::sync::Arc;

/// Tool: start a background job. See the module docs for lifecycle.
pub struct ShellSpawn {
    table: Arc<JobTable>,
    schema: ToolSchema,
}

impl ShellSpawn {
    pub fn new(table: Arc<JobTable>) -> Self {
        Self {
            table,
            schema: ToolSchema {
                name: "shell_spawn".into(),
                description: "Start a long-running command in the background (dev server, \
                              watcher, anything that does not exit on its own) and return a \
                              job_id immediately. Commands that finish within ~2s are returned \
                              directly like shell_exec. Poll with shell_job_status, stop with \
                              shell_job_kill. scope: 'run' (default — killed automatically when \
                              this task ends), 'session' (survives into later turns of this \
                              conversation, reclaimed on idle timeout), 'detached' (outlives \
                              everything; only when the user explicitly asks for that)."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "program": {"type": "string"},
                        "args": {"type": "array", "items": {"type": "string"}},
                        "cwd": {"type": "string", "description": "Relative to workspace root"},
                        "scope": {"type": "string", "enum": ["run", "session", "detached"]}
                    },
                    "required": ["program"]
                }),
            },
        }
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
                              plus everything it printed since you last checked. Without an id, \
                              lists all your jobs."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": { "id": {"type": "integer"} }
                }),
            },
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
        match args.get("id").and_then(Value::as_u64) {
            Some(id) => match self.table.status(id, world) {
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
/// Stuck and BudgetExhausted), reap this run's `run`-scoped jobs.
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
