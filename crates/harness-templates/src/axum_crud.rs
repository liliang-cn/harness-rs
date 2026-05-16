//! `axum-crud` template — for building / maintaining a CRUD service on axum + sqlx.
//!
//! What you get:
//! - **Blueprint** with deterministic format/clippy/test nodes wrapping an agent node
//!   that performs the actual write work.
//! - **`tools()`** returns the recommended toolset (read_file, write_file, edit_file,
//!   list_dir, shell_read).
//! - **`sensors()`** returns CargoCheck + Clippy.
//! - **`guides()`** returns Markdown guides describing axum / sqlx / tracing
//!   conventions; these get pushed onto Context.guides before the agent runs.

use async_trait::async_trait;
use harness_blueprint::{Blueprint, Node, NodeOutput, Transition};
use harness_core::{
    Context, Execution, Guide, GuideError, GuideId, GuideScope, HarnessError, Sensor, Tool, World,
};
use harness_sensors_rust::{CargoCheck, Clippy};
use harness_tools_fs::{EditFile, ListDir, ReadFile, WriteFile};
use harness_tools_shell::ShellRead;
use std::sync::Arc;

/// Recommended toolset for the axum-crud workflow.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFile),
        Arc::new(WriteFile),
        Arc::new(EditFile),
        Arc::new(ListDir),
        Arc::new(ShellRead),
    ]
}

/// Recommended sensors: cargo check + clippy.
pub fn sensors() -> Vec<Arc<dyn Sensor>> {
    vec![Arc::new(CargoCheck::new()), Arc::new(Clippy::new())]
}

/// Curated guides shaped for axum + sqlx + tracing conventions.
pub fn guides() -> Vec<Arc<dyn Guide>> {
    vec![Arc::new(AxumConventions), Arc::new(SqlxConventions), Arc::new(TracingConventions)]
}

/// A self-contained Blueprint: deterministic git status → agent work →
/// fmt → clippy → test → done. The agent node is supplied by the caller because
/// it needs a model.
pub fn blueprint<F>(agent_step: F) -> Blueprint
where
    F: for<'a> Fn(&'a mut World) -> futures::future::BoxFuture<'a, Result<NodeOutput, HarnessError>>
        + Send
        + Sync
        + 'static,
{
    Blueprint::new()
        .add("status", Node::deterministic(|w| Box::pin(async move {
            let out = w.runner.exec("git", &["status", "--short"], Some(w.repo.root.as_path())).await
                .map_err(|e| HarnessError::Other(e.to_string()))?;
            Ok(NodeOutput {
                transition: Transition::Next,
                data: serde_json::json!({"git_status": out.stdout}),
            })
        })))
        .add("work", Node::agent(agent_step))
        .add("fmt", Node::deterministic(|w| Box::pin(async move {
            let _ = w.runner.exec("cargo", &["fmt", "--all"], Some(w.repo.root.as_path())).await;
            Ok(NodeOutput { transition: Transition::Next, data: serde_json::json!({"fmt": "ok"}) })
        })))
        .add("clippy", Node::deterministic(|w| Box::pin(async move {
            let out = w.runner.exec(
                "cargo",
                &["clippy", "--quiet", "--", "-D", "warnings"],
                Some(w.repo.root.as_path()),
            ).await.map_err(|e| HarnessError::Other(e.to_string()))?;
            if out.status != 0 {
                return Err(HarnessError::Other(format!("clippy failed: {}", out.stderr)));
            }
            Ok(NodeOutput { transition: Transition::Next, data: serde_json::json!({"clippy": "ok"}) })
        })))
        .add("test", Node::deterministic(|w| Box::pin(async move {
            let out = w.runner.exec("cargo", &["test", "--quiet"], Some(w.repo.root.as_path())).await
                .map_err(|e| HarnessError::Other(e.to_string()))?;
            Ok(NodeOutput {
                transition: Transition::Done,
                data: serde_json::json!({"test_status": out.status}),
            })
        })))
        .edge("status", "work")
        .edge("work", "fmt")
        .edge("fmt", "clippy")
        .edge("clippy", "test")
        .branch_on_failure("clippy", "work", 2)
        .branch_on_failure("test",   "work", 2)
}

// ---------- guides ----------

const AXUM_GUIDE_BODY: &str = "## axum conventions\n\
- Every handler returns `Result<impl IntoResponse, AppError>`. Never `unwrap`.\n\
- Routes are declared in `src/api/<resource>.rs` and mounted in `src/api/mod.rs`.\n\
- Request bodies use `axum::Json<T>` with a `#[derive(Deserialize, Validate)]` type.\n\
- Path/query extractors live above the body extractor in handler signatures.\n\
- Use `axum::extract::State` for application state, never globals.\n";

const SQLX_GUIDE_BODY: &str = "## sqlx conventions\n\
- Prefer `query_as!` macros over runtime query strings — compile-time SQL validation.\n\
- All queries run inside `sqlx::PgPool` instances threaded through `State`.\n\
- Migrations live under `migrations/`; never edit a merged migration in-place.\n\
- Use transactions (`pool.begin()`) for any multi-statement write.\n";

const TRACING_GUIDE_BODY: &str = "## tracing conventions\n\
- Each handler is wrapped with `#[tracing::instrument(skip(state))]`.\n\
- Use `tracing::info!` for service-level events, `tracing::debug!` for inner detail.\n\
- Errors propagate via `#[from]` on AppError variants; the handler's tracing span captures them automatically.\n\
- Never `println!` — it bypasses the subscriber.\n";

pub struct AxumConventions;
#[async_trait]
impl Guide for AxumConventions {
    fn id(&self) -> &GuideId {
        static I: std::sync::OnceLock<GuideId> = std::sync::OnceLock::new();
        I.get_or_init(|| "axum-conventions".into())
    }
    fn kind(&self) -> Execution { Execution::Inferential }
    fn scope(&self) -> &GuideScope {
        static S: std::sync::OnceLock<GuideScope> = std::sync::OnceLock::new();
        S.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _world: &World) -> Result<(), GuideError> {
        ctx.guides.push(harness_core::Block::Text(AXUM_GUIDE_BODY.into()));
        Ok(())
    }
}

pub struct SqlxConventions;
#[async_trait]
impl Guide for SqlxConventions {
    fn id(&self) -> &GuideId {
        static I: std::sync::OnceLock<GuideId> = std::sync::OnceLock::new();
        I.get_or_init(|| "sqlx-conventions".into())
    }
    fn kind(&self) -> Execution { Execution::Inferential }
    fn scope(&self) -> &GuideScope {
        static S: std::sync::OnceLock<GuideScope> = std::sync::OnceLock::new();
        S.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _world: &World) -> Result<(), GuideError> {
        ctx.guides.push(harness_core::Block::Text(SQLX_GUIDE_BODY.into()));
        Ok(())
    }
}

pub struct TracingConventions;
#[async_trait]
impl Guide for TracingConventions {
    fn id(&self) -> &GuideId {
        static I: std::sync::OnceLock<GuideId> = std::sync::OnceLock::new();
        I.get_or_init(|| "tracing-conventions".into())
    }
    fn kind(&self) -> Execution { Execution::Inferential }
    fn scope(&self) -> &GuideScope {
        static S: std::sync::OnceLock<GuideScope> = std::sync::OnceLock::new();
        S.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _world: &World) -> Result<(), GuideError> {
        ctx.guides.push(harness_core::Block::Text(TRACING_GUIDE_BODY.into()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Task;

    #[tokio::test]
    async fn guides_inject_text_blocks() {
        let world = harness_context::default_world(".");
        let mut ctx = Context::new(Task {
            description: "t".into(), source: None, deadline: None,
        });
        for g in guides() {
            g.apply(&mut ctx, &world).await.unwrap();
        }
        assert_eq!(ctx.guides.len(), 3);
    }

    #[test]
    fn tools_and_sensors_are_built() {
        assert_eq!(tools().len(), 5);
        assert_eq!(sensors().len(), 2);
    }
}
