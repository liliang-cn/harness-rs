//! [`ModelRouter`] — compose two [`Model`]s into a "local-first, cloud-fallback"
//! policy without a per-vendor menu.
//!
//! The single-machine SMB deployment this targets wants: run everyday requests
//! on a **local** model (data never leaves the intranet), but let a **cloud**
//! model bail it out when the local one errors, and let a request opt into the
//! cloud when it genuinely needs the stronger model — *unless* the data is
//! sensitive, in which case it must stay local no matter what.
//!
//! `ModelRouter` is itself a [`Model`], so it drops into `AgentLoop::new(router)`
//! exactly where a single model would. It composes models (the framework's
//! principle: no hardcoded URLs, no vendor switch — you build each leg with
//! [`crate::ApiKind`] and hand them in).
//!
//! # Routing, in order
//! 1. **keep-local guard** — if it fires, the request runs on `primary` (local)
//!    and can never fall back to cloud. This overrides everything below.
//! 2. **selector** — otherwise pick `Primary` or `Fallback` for this request.
//! 3. **failover** — if the chosen leg errors and failover is on, retry once on
//!    the other leg (subject to the keep-local guard).
//!
//! Both the guard and the selector default to reading [`Context::metadata`], so
//! a serving layer just sets a flag per request:
//!
//! ```ignore
//! use harness_models::{ApiKind, ModelRouter, KEEP_LOCAL_KEY, PREFER_FALLBACK_KEY};
//!
//! let local = ApiKind::OpenAI.build("http://localhost:11434/v1", "qwen2.5:14b", "ollama");
//! let cloud = ApiKind::Anthropic.build("https://api.anthropic.com", "claude-opus-4-8", key);
//! let router = ModelRouter::new(local).with_fallback(cloud); // failover on by default
//!
//! // In the serving layer, per request:
//! ctx.metadata.insert(KEEP_LOCAL_KEY.into(), true.into());        // HR data → stay local
//! ctx.metadata.insert(PREFER_FALLBACK_KEY.into(), true.into());   // hard BI query → prefer cloud
//! ```

use harness_core::error::ModelError;
use harness_core::{Context, Model, ModelDelta, ModelInfo, ModelOutput};
use std::sync::Arc;

/// `Context.metadata` key: when truthy, the request must run on the **primary
/// (local)** model and must never fall back to the cloud leg — even on error.
/// Set this for data that must not leave the intranet.
pub const KEEP_LOCAL_KEY: &str = "router.keep_local";

/// `Context.metadata` key: when truthy, prefer the **fallback (cloud/strong)**
/// leg for this request. Ignored when [`KEEP_LOCAL_KEY`] is also set.
pub const PREFER_FALLBACK_KEY: &str = "router.prefer_fallback";

/// Which leg handles a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The primary (typically local) model.
    Primary,
    /// The fallback (typically cloud/strong) model.
    Fallback,
}

type Selector = Box<dyn Fn(&Context) -> Route + Send + Sync>;
type LocalGuard = Box<dyn Fn(&Context) -> bool + Send + Sync>;

/// Reads `ctx.metadata[key]` as a boolean, defaulting to `false`.
fn meta_flag(ctx: &Context, key: &str) -> bool {
    ctx.metadata
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// A [`Model`] that routes each request between a primary and an optional
/// fallback leg. See the [module docs](self).
pub struct ModelRouter {
    primary: Arc<dyn Model>,
    fallback: Option<Arc<dyn Model>>,
    select: Selector,
    keep_local: LocalGuard,
    failover: bool,
}

impl ModelRouter {
    /// A router over `primary` alone — no fallback, no failover. Add a cloud leg
    /// with [`with_fallback`](Self::with_fallback).
    pub fn new(primary: Arc<dyn Model>) -> Self {
        Self {
            primary,
            fallback: None,
            // Default selector: honor the PREFER_FALLBACK metadata flag.
            select: Box::new(|ctx| {
                if meta_flag(ctx, PREFER_FALLBACK_KEY) {
                    Route::Fallback
                } else {
                    Route::Primary
                }
            }),
            // Default guard: honor the KEEP_LOCAL metadata flag.
            keep_local: Box::new(|ctx| meta_flag(ctx, KEEP_LOCAL_KEY)),
            failover: false,
        }
    }

    /// Attach the fallback (cloud/strong) leg. Turns **failover on** by default;
    /// disable with [`with_failover(false)`](Self::with_failover).
    pub fn with_fallback(mut self, fallback: Arc<dyn Model>) -> Self {
        self.fallback = Some(fallback);
        self.failover = true;
        self
    }

    /// Enable or disable single-retry failover onto the other leg when the
    /// chosen one errors. No effect without a fallback. A keep-local request
    /// never fails over to the cloud regardless of this setting.
    pub fn with_failover(mut self, on: bool) -> Self {
        self.failover = on;
        self
    }

    /// Override the per-request leg selector. Runs only when the keep-local
    /// guard did not force the primary. Default: read [`PREFER_FALLBACK_KEY`].
    pub fn route_with(mut self, f: impl Fn(&Context) -> Route + Send + Sync + 'static) -> Self {
        self.select = Box::new(f);
        self
    }

    /// Override the keep-local guard: return `true` to pin a request to the
    /// primary (local) leg with no cloud fallback. Default: read
    /// [`KEEP_LOCAL_KEY`]. Compose your own, e.g. inspect the task text or the
    /// tools in play.
    pub fn keep_local_when(mut self, f: impl Fn(&Context) -> bool + Send + Sync + 'static) -> Self {
        self.keep_local = Box::new(f);
        self
    }

    /// Resolve `(chosen, failover_target)` for this request. `failover_target`
    /// is `None` when the request is pinned local or no fallback exists.
    fn pick(&self, ctx: &Context) -> (Arc<dyn Model>, Option<Arc<dyn Model>>) {
        let local_only = (self.keep_local)(ctx);
        let want = if local_only {
            Route::Primary
        } else {
            (self.select)(ctx)
        };
        match want {
            Route::Primary => {
                let backup = if local_only {
                    None
                } else {
                    self.fallback.clone()
                };
                (self.primary.clone(), backup)
            }
            Route::Fallback => match &self.fallback {
                // Fallback errors can always retreat to the (safe) local leg.
                Some(fb) => (fb.clone(), Some(self.primary.clone())),
                None => (self.primary.clone(), None),
            },
        }
    }
}

#[async_trait::async_trait]
impl Model for ModelRouter {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let (chosen, backup) = self.pick(ctx);
        match chosen.complete(ctx).await {
            Ok(out) => Ok(out),
            Err(e) => match backup {
                Some(b) if self.failover => {
                    tracing::warn!(
                        target: "harness.router",
                        error = %e,
                        "primary leg failed; failing over to the other model",
                    );
                    b.complete(ctx).await
                }
                _ => Err(e),
            },
        }
    }

    async fn stream(
        &self,
        ctx: &Context,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>
    {
        // Failover applies only to stream *setup* (before any delta is emitted);
        // a mid-stream provider error is surfaced to the caller, not retried.
        let (chosen, backup) = self.pick(ctx);
        match chosen.stream(ctx).await {
            Ok(s) => Ok(s),
            Err(e) => match backup {
                Some(b) if self.failover => {
                    tracing::warn!(
                        target: "harness.router",
                        error = %e,
                        "primary leg failed at stream setup; failing over",
                    );
                    b.stream(ctx).await
                }
                _ => Err(e),
            },
        }
    }

    /// Reports the **primary** model's info: it handles the common path and its
    /// context window is the binding constraint for local-first deployments.
    fn info(&self) -> ModelInfo {
        let mut info = self.primary.info();
        info.handle = format!("router:{}", info.handle);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockModel, MockResponse};
    use harness_core::{Context, Task};

    /// A model that always errors — stands in for an unreachable local server.
    struct FailModel;
    #[async_trait::async_trait]
    impl Model for FailModel {
        async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
            Err(ModelError::Transport("local server down".into()))
        }
        fn info(&self) -> ModelInfo {
            MockModel::new().info()
        }
    }

    fn ctx_with(flags: &[(&str, bool)]) -> Context {
        let mut c = Context::new(Task {
            description: "q".into(),
            source: None,
            deadline: None,
        });
        for (k, v) in flags {
            c.metadata.insert((*k).into(), (*v).into());
        }
        c
    }

    fn local() -> Arc<dyn Model> {
        Arc::new(MockModel::new().script(MockResponse::text("LOCAL")))
    }
    fn cloud() -> Arc<dyn Model> {
        Arc::new(MockModel::new().script(MockResponse::text("CLOUD")))
    }

    #[tokio::test]
    async fn defaults_to_primary() {
        let r = ModelRouter::new(local()).with_fallback(cloud());
        let out = r.complete(&ctx_with(&[])).await.unwrap();
        assert_eq!(out.text.as_deref(), Some("LOCAL"));
    }

    #[tokio::test]
    async fn prefer_fallback_flag_routes_to_cloud() {
        let r = ModelRouter::new(local()).with_fallback(cloud());
        let out = r
            .complete(&ctx_with(&[(PREFER_FALLBACK_KEY, true)]))
            .await
            .unwrap();
        assert_eq!(out.text.as_deref(), Some("CLOUD"));
    }

    #[tokio::test]
    async fn keep_local_overrides_prefer_fallback() {
        let r = ModelRouter::new(local()).with_fallback(cloud());
        // Both flags set: keep-local wins, request stays on the local leg.
        let out = r
            .complete(&ctx_with(&[
                (PREFER_FALLBACK_KEY, true),
                (KEEP_LOCAL_KEY, true),
            ]))
            .await
            .unwrap();
        assert_eq!(out.text.as_deref(), Some("LOCAL"));
    }

    #[tokio::test]
    async fn failover_when_primary_errors() {
        let r = ModelRouter::new(Arc::new(FailModel)).with_fallback(cloud());
        let out = r.complete(&ctx_with(&[])).await.unwrap();
        assert_eq!(
            out.text.as_deref(),
            Some("CLOUD"),
            "should fail over to cloud"
        );
    }

    #[tokio::test]
    async fn keep_local_forbids_cloud_failover() {
        // Local server down + keep-local set → error, never touches the cloud.
        let r = ModelRouter::new(Arc::new(FailModel)).with_fallback(cloud());
        let res = r.complete(&ctx_with(&[(KEEP_LOCAL_KEY, true)])).await;
        assert!(res.is_err(), "keep-local must not fail over to cloud");
    }

    #[tokio::test]
    async fn no_failover_when_disabled() {
        let r = ModelRouter::new(Arc::new(FailModel))
            .with_fallback(cloud())
            .with_failover(false);
        assert!(r.complete(&ctx_with(&[])).await.is_err());
    }

    #[test]
    fn info_reports_primary_with_router_prefix() {
        let r = ModelRouter::new(local()).with_fallback(cloud());
        assert!(r.info().handle.starts_with("router:"));
    }
}
