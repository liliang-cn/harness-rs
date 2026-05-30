//! Background-task primitives for harness-rs agents.
//!
//! Gives the LLM four tools — `tasks_create`, `tasks_list`, `tasks_get`,
//! `tasks_cancel` — backed by a swappable [`TaskStore`]. Combined with
//! `harness-rs-daemon` (or any cron runner) you get an "agent dispatches
//! deferred work" pattern: the chat session enqueues a task; an external
//! runner picks it up on schedule and (typically) shells out to the same
//! agent binary in headless mode.
//!
//! The crate is **execution-agnostic** — it persists the task descriptor
//! and exposes inspection tools. Pair with `harness-rs-daemon`'s file
//! source (or your own runner) to actually invoke `argv` on `schedule`.
//!
//! # Example
//!
//! ```ignore
//! use harness_tools_tasks::{JsonFileStore, make_tools};
//! use std::sync::Arc;
//!
//! let store = Arc::new(JsonFileStore::new("/var/lib/myapp/tasks.json"));
//! let task_tools = make_tools(store);
//! for t in task_tools {
//!     loop_ = loop_.with_tool(t);
//! }
//! ```

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum TaskStoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

/// A deferred task descriptor. `argv` is the canonical "what to run"; the
/// store does NOT execute it — that's a runner's job. `schedule` is a free-
/// form spec (e.g. `daily 08:00`, `every 1h`, cron) that the runner
/// interprets; the store treats it as an opaque string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// Optional user / tenant scoping. The 4 tools auto-filter by this when
    /// the agent's `World.profile.extra["user_id"]` is set.
    pub user_id: Option<String>,
    /// Short label shown in `tasks_list`.
    pub name: String,
    /// `one_off` | `recurring`.
    pub kind: String,
    /// argv to execute (runner shells out): `["ledger", "--brief"]`.
    pub argv: Vec<String>,
    /// Schedule spec for recurring tasks; `None` for one_off (runs ASAP).
    pub schedule: Option<String>,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub next_run_at: Option<DateTime<Utc>>,
    /// Captured output from the last run. Truncated by the runner if huge.
    pub last_output: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// Input to `TaskStore::create`. The store fills in id / status /
/// timestamps.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub user_id: Option<String>,
    pub name: String,
    pub kind: String,
    pub argv: Vec<String>,
    pub schedule: Option<String>,
}

/// Optional filter for `TaskStore::list`.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub user_id: Option<String>,
    pub status: Option<TaskStatus>,
}

#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    async fn create(&self, n: NewTask) -> Result<Task, TaskStoreError>;
    async fn get(&self, id: &str) -> Result<Option<Task>, TaskStoreError>;
    async fn list(&self, filter: &TaskFilter) -> Result<Vec<Task>, TaskStoreError>;
    async fn cancel(&self, id: &str) -> Result<(), TaskStoreError>;
}

// ───── default impl: JSON file ─────

/// Simple JSON-file-backed store. Whole file is locked on every write, so
/// it's not fast — but it's dependency-free and works fine for the typical
/// "a handful of tasks per user" case. Use a SQLite-backed `TaskStore`
/// impl for higher throughput.
pub struct JsonFileStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonFileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    async fn load(&self) -> Result<Vec<Task>, TaskStoreError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) if !bytes.is_empty() => {
                serde_json::from_slice::<Vec<Task>>(&bytes).map_err(|e| {
                    TaskStoreError::Invalid(format!("parse {}: {e}", self.path.display()))
                })
            }
            Ok(_) => Ok(Vec::new()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(TaskStoreError::Io(e.to_string())),
        }
    }

    async fn save(&self, tasks: &[Task]) -> Result<(), TaskStoreError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| TaskStoreError::Io(format!("mkdir: {e}")))?;
        }
        let bytes = serde_json::to_vec_pretty(tasks)
            .map_err(|e| TaskStoreError::Invalid(format!("serialise: {e}")))?;
        tokio::fs::write(&self.path, &bytes)
            .await
            .map_err(|e| TaskStoreError::Io(format!("write {}: {e}", self.path.display())))
    }
}

#[async_trait]
impl TaskStore for JsonFileStore {
    async fn create(&self, n: NewTask) -> Result<Task, TaskStoreError> {
        let _g = self.lock.lock().await;
        let mut all = self.load().await?;
        let t = Task {
            id: uuid::Uuid::new_v4().to_string()[..8].into(),
            user_id: n.user_id,
            name: n.name,
            kind: n.kind,
            argv: n.argv,
            schedule: n.schedule,
            status: TaskStatus::Pending,
            created_at: Utc::now(),
            last_run_at: None,
            next_run_at: None,
            last_output: None,
        };
        all.push(t.clone());
        self.save(&all).await?;
        Ok(t)
    }

    async fn get(&self, id: &str) -> Result<Option<Task>, TaskStoreError> {
        let all = self.load().await?;
        Ok(all.into_iter().find(|t| t.id == id))
    }

    async fn list(&self, f: &TaskFilter) -> Result<Vec<Task>, TaskStoreError> {
        let all = self.load().await?;
        Ok(all
            .into_iter()
            .filter(|t| {
                f.user_id
                    .as_deref()
                    .is_none_or(|u| t.user_id.as_deref() == Some(u))
            })
            .filter(|t| f.status.is_none_or(|s| t.status == s))
            .collect())
    }

    async fn cancel(&self, id: &str) -> Result<(), TaskStoreError> {
        let _g = self.lock.lock().await;
        let mut all = self.load().await?;
        let t = all
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| TaskStoreError::NotFound(id.into()))?;
        t.status = TaskStatus::Cancelled;
        self.save(&all).await
    }
}

// ───── tools ─────

fn uid_of(world: &World) -> Option<String> {
    world.profile.extra::<String>("user_id")
}

pub struct TasksCreateTool {
    store: Arc<dyn TaskStore>,
    schema: ToolSchema,
}

impl TasksCreateTool {
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        Self {
            store,
            schema: ToolSchema {
                name: "tasks_create".into(),
                description: "Queue a deferred / scheduled task. The store persists \
                              the descriptor; an external runner (e.g. harness-rs-daemon) \
                              picks it up and shells out to `argv`. Use this when the user \
                              asks for something recurring (\"每月 1 号生成账单\") or for \
                              a follow-up at a later time."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "name":     {"type": "string", "description": "Short human label, e.g. \"monthly_brief\"."},
                        "kind":     {"type": "string", "enum": ["one_off", "recurring"]},
                        "argv":     {"type": "array", "items": {"type": "string"}, "description": "Command argv to run, e.g. [\"ledger\", \"--brief\"]."},
                        "schedule": {"type": "string", "description": "For recurring: daemon-friendly schedule, e.g. \"daily 08:00\", \"every 1h\", or cron. Omit for one_off."}
                    },
                    "required": ["name", "kind", "argv"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TasksCreateTool {
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
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "tasks_create".into(),
                reason: "name required".into(),
            })?
            .to_string();
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "tasks_create".into(),
                reason: "kind required".into(),
            })?
            .to_string();
        if kind != "one_off" && kind != "recurring" {
            return Err(ToolError::InvalidArgs {
                name: "tasks_create".into(),
                reason: format!("kind must be one_off|recurring, got `{kind}`"),
            });
        }
        let argv: Vec<String> = args
            .get("argv")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "tasks_create".into(),
                reason: "argv array required".into(),
            })?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if argv.is_empty() {
            return Err(ToolError::InvalidArgs {
                name: "tasks_create".into(),
                reason: "argv must not be empty".into(),
            });
        }
        let schedule = args
            .get("schedule")
            .and_then(|v| v.as_str())
            .map(String::from);
        if kind == "recurring" && schedule.is_none() {
            return Err(ToolError::InvalidArgs {
                name: "tasks_create".into(),
                reason: "recurring tasks require a schedule".into(),
            });
        }
        let new = NewTask {
            user_id: uid_of(world),
            name,
            kind,
            argv,
            schedule,
        };
        let t = self
            .store
            .create(new)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        Ok(ToolResult {
            ok: true,
            content: json!(t),
            trace: None,
        })
    }
}

pub struct TasksListTool {
    store: Arc<dyn TaskStore>,
    schema: ToolSchema,
}

impl TasksListTool {
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        Self {
            store,
            schema: ToolSchema {
                name: "tasks_list".into(),
                description: "List queued / running / done tasks for the current user. \
                              Optional status filter."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "status": {"type": "string", "enum": ["pending", "running", "done", "failed", "cancelled"]}
                    }
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TasksListTool {
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
        let status = args
            .get("status")
            .and_then(|v| v.as_str())
            .and_then(|s| match s {
                "pending" => Some(TaskStatus::Pending),
                "running" => Some(TaskStatus::Running),
                "done" => Some(TaskStatus::Done),
                "failed" => Some(TaskStatus::Failed),
                "cancelled" => Some(TaskStatus::Cancelled),
                _ => None,
            });
        let filter = TaskFilter {
            user_id: uid_of(world),
            status,
        };
        let tasks = self
            .store
            .list(&filter)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        Ok(ToolResult {
            ok: true,
            content: json!({"count": tasks.len(), "tasks": tasks}),
            trace: None,
        })
    }
}

pub struct TasksGetTool {
    store: Arc<dyn TaskStore>,
    schema: ToolSchema,
}

impl TasksGetTool {
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        Self {
            store,
            schema: ToolSchema {
                name: "tasks_get".into(),
                description: "Fetch a single task by id (status, last_output, next_run_at).".into(),
                input: json!({
                    "type": "object",
                    "properties": { "id": {"type": "string"} },
                    "required": ["id"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TasksGetTool {
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
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "tasks_get".into(),
                reason: "id required".into(),
            })?;
        let t = self
            .store
            .get(id)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        // Multi-tenant guard: don't leak tasks across users.
        let uid = uid_of(world);
        let allowed = match (&t, &uid) {
            (Some(task), Some(u)) => task.user_id.as_deref() == Some(u.as_str()),
            (Some(task), None) => task.user_id.is_none(),
            (None, _) => true,
        };
        if !allowed {
            return Ok(ToolResult {
                ok: false,
                content: json!({"error": "not found"}),
                trace: None,
            });
        }
        Ok(ToolResult {
            ok: t.is_some(),
            content: match t {
                Some(task) => json!(task),
                None => json!({"error": format!("no task with id `{id}`")}),
            },
            trace: None,
        })
    }
}

pub struct TasksCancelTool {
    store: Arc<dyn TaskStore>,
    schema: ToolSchema,
}

impl TasksCancelTool {
    pub fn new(store: Arc<dyn TaskStore>) -> Self {
        Self {
            store,
            schema: ToolSchema {
                name: "tasks_cancel".into(),
                description: "Cancel a queued / recurring task by id. The runner stops \
                              firing it; the descriptor stays in the store for history."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": { "id": {"type": "string"} },
                    "required": ["id"]
                }),
            },
        }
    }
}

#[async_trait]
impl Tool for TasksCancelTool {
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
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "tasks_cancel".into(),
                reason: "id required".into(),
            })?;
        // Tenant guard: ensure the task belongs to this user before cancelling.
        let uid = uid_of(world);
        if let Ok(Some(task)) = self.store.get(id).await {
            let allowed = match (&task.user_id, &uid) {
                (Some(a), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            };
            if !allowed {
                return Ok(ToolResult {
                    ok: false,
                    content: json!({"error": "not found"}),
                    trace: None,
                });
            }
        }
        self.store
            .cancel(id)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        Ok(ToolResult {
            ok: true,
            content: json!({"cancelled": id}),
            trace: None,
        })
    }
}

/// Build all four task tools at once. Useful in the typical case where you
/// pass them straight into `AgentLoop::with_tool(...)` in a loop.
pub fn make_tools(store: Arc<dyn TaskStore>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(TasksCreateTool::new(store.clone())),
        Arc::new(TasksListTool::new(store.clone())),
        Arc::new(TasksGetTool::new(store.clone())),
        Arc::new(TasksCancelTool::new(store)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn json_store_roundtrips_tasks() {
        let dir = tempdir().unwrap();
        let store = JsonFileStore::new(dir.path().join("tasks.json"));
        let t = store
            .create(NewTask {
                user_id: Some("u1".into()),
                name: "brief".into(),
                kind: "recurring".into(),
                argv: vec!["ledger".into(), "--brief".into()],
                schedule: Some("daily 08:00".into()),
            })
            .await
            .unwrap();
        assert_eq!(t.status, TaskStatus::Pending);
        let got = store.get(&t.id).await.unwrap().unwrap();
        assert_eq!(got.name, "brief");
        let listed = store.list(&TaskFilter::default()).await.unwrap();
        assert_eq!(listed.len(), 1);
        store.cancel(&t.id).await.unwrap();
        let cancelled = store.get(&t.id).await.unwrap().unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn list_filters_by_user_and_status() {
        let dir = tempdir().unwrap();
        let store = JsonFileStore::new(dir.path().join("tasks.json"));
        store
            .create(NewTask {
                user_id: Some("u1".into()),
                name: "a".into(),
                kind: "one_off".into(),
                argv: vec!["echo".into(), "a".into()],
                schedule: None,
            })
            .await
            .unwrap();
        store
            .create(NewTask {
                user_id: Some("u2".into()),
                name: "b".into(),
                kind: "one_off".into(),
                argv: vec!["echo".into(), "b".into()],
                schedule: None,
            })
            .await
            .unwrap();
        let u1 = store
            .list(&TaskFilter {
                user_id: Some("u1".into()),
                status: None,
            })
            .await
            .unwrap();
        assert_eq!(u1.len(), 1);
        assert_eq!(u1[0].user_id.as_deref(), Some("u1"));
    }
}
