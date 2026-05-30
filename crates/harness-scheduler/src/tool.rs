//! `cronjob` — lets an agent schedule its own recurring jobs. The agent supplies
//! the schedule STRING (validated by `harness_daemon::Schedule::parse`); there is
//! no LLM-parsing step.

use crate::store::{Job, JobStore};
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use harness_daemon::Schedule;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct CronjobTool {
    store: Arc<dyn JobStore>,
    schema: ToolSchema,
}

impl CronjobTool {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self {
            store,
            schema: ToolSchema {
                name: "cronjob".into(),
                description: "Schedule recurring agent jobs. actions: create (name, \
                    schedule, prompt, channel, target?), list, remove (id), pause (id), \
                    resume (id). schedule is a string: 'daily 08:00', 'weekly mon 09:30', \
                    or 'every 15m'. When the job fires, your `prompt` runs as a task and \
                    the result is delivered to `channel`."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "list", "remove", "pause", "resume"]},
                        "name": {"type": "string"},
                        "schedule": {"type": "string", "description": "'daily 08:00' | 'weekly mon 09:30' | 'every 15m'"},
                        "prompt": {"type": "string", "description": "task to run at fire time"},
                        "channel": {"type": "string", "description": "delivery channel key (default 'stdout')"},
                        "target": {"type": "string", "description": "channel recipient (email address, etc.)"},
                        "id": {"type": "string", "description": "job id for remove/pause/resume"}
                    },
                    "required": ["action"]
                }),
            },
        }
    }

    fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
        a.get(k).and_then(|v| v.as_str())
    }
}

#[async_trait]
impl Tool for CronjobTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Destructive
    }

    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let action = Self::s(&args, "action").ok_or_else(|| ToolError::InvalidArgs {
            name: "cronjob".into(),
            reason: "action required".into(),
        })?;
        let res: Result<Value, String> = match action {
            "create" => {
                let name = Self::s(&args, "name").unwrap_or("job");
                let schedule =
                    Self::s(&args, "schedule").ok_or_else(|| ToolError::InvalidArgs {
                        name: "cronjob".into(),
                        reason: "schedule required".into(),
                    })?;
                let prompt = Self::s(&args, "prompt").ok_or_else(|| ToolError::InvalidArgs {
                    name: "cronjob".into(),
                    reason: "prompt required".into(),
                })?;
                let channel = Self::s(&args, "channel").unwrap_or("stdout");
                let target = Self::s(&args, "target").map(|s| s.to_string());
                match Schedule::parse(schedule) {
                    Ok(sched) => {
                        let now = chrono::Local::now();
                        let next = sched.next_after(now).timestamp_millis();
                        let job = Job::new(name, schedule, prompt, channel, now.timestamp_millis())
                            .with_target(target)
                            .with_next_run(Some(next));
                        let id = job.id.clone();
                        self.store
                            .add(&job)
                            .await
                            .map_err(|e| e.to_string())
                            .map(|_| json!({"created": id, "next_run_ms": next}))
                    }
                    Err(e) => Err(format!("bad schedule `{schedule}`: {e}")),
                }
            }
            "list" => self
                .store
                .list()
                .await
                .map(|jobs| json!({"jobs": jobs}))
                .map_err(|e| e.to_string()),
            "remove" => {
                let id = Self::s(&args, "id").ok_or_else(|| ToolError::InvalidArgs {
                    name: "cronjob".into(),
                    reason: "id required".into(),
                })?;
                self.store
                    .remove(id)
                    .await
                    .map(|r| json!({"removed": r}))
                    .map_err(|e| e.to_string())
            }
            "pause" | "resume" => {
                let id = Self::s(&args, "id").ok_or_else(|| ToolError::InvalidArgs {
                    name: "cronjob".into(),
                    reason: "id required".into(),
                })?;
                let on = action == "resume";
                self.store
                    .set_enabled(id, on)
                    .await
                    .map(|r| json!({"updated": r, "enabled": on}))
                    .map_err(|e| e.to_string())
            }
            other => Err(format!("unknown action `{other}`")),
        };
        match res {
            Ok(content) => Ok(ToolResult {
                ok: true,
                content,
                trace: None,
            }),
            Err(reason) => Ok(ToolResult {
                ok: false,
                content: json!({"error": reason}),
                trace: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileJobStore;
    use harness_context::default_world;

    fn tmp() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let c = N.fetch_add(1, Ordering::SeqCst);
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "harness-cronjob-{}-{n}-{c}.json",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn create_validates_and_lists_and_removes() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        let tool = CronjobTool::new(store.clone());
        let mut w = default_world(".");

        let out = tool
            .invoke(
                json!({"action":"create","name":"x","schedule":"nonsense","prompt":"p"}),
                &mut w,
            )
            .await
            .unwrap();
        assert!(!out.ok, "bad schedule must be rejected");

        let out = tool.invoke(json!({"action":"create","name":"brief","schedule":"daily 08:00","prompt":"write brief","channel":"stdout"}), &mut w).await.unwrap();
        assert!(out.ok);
        let id = out.content["created"].as_str().unwrap().to_string();
        assert!(out.content["next_run_ms"].as_i64().unwrap() > 0);

        let out = tool.invoke(json!({"action":"list"}), &mut w).await.unwrap();
        assert_eq!(out.content["jobs"].as_array().unwrap().len(), 1);

        let out = tool
            .invoke(json!({"action":"pause","id": id}), &mut w)
            .await
            .unwrap();
        assert!(out.ok);
        assert!(!store.get(&id).await.unwrap().unwrap().enabled);

        let out = tool
            .invoke(json!({"action":"remove","id": id}), &mut w)
            .await
            .unwrap();
        assert!(out.ok);
        assert!(store.list().await.unwrap().is_empty());

        let _ = std::fs::remove_file(&p);
    }
}
