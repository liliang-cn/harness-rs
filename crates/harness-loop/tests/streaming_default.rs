//! Streaming is the loop's default transport; non-streaming is an explicit
//! opt-out. Every model request the loop makes, including the forced final
//! synthesis, goes through `Model::stream()` unless `with_streaming(false)`.

use harness_context::default_world;
use harness_core::{Context, Model, ModelDelta, ModelError, ModelInfo, ModelOutput, Task};
use harness_loop::AgentLoop;
use harness_models::{MockModel, MockResponse};
use harness_tools::fs::ReadFile;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Counts how each request arrived.
struct Counting {
    inner: MockModel,
    direct_complete: Arc<AtomicU32>,
    streams: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl Model for Counting {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.direct_complete.fetch_add(1, Ordering::SeqCst);
        self.inner.complete(ctx).await
    }
    async fn stream(
        &self,
        ctx: &Context,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>
    {
        self.streams.fetch_add(1, Ordering::SeqCst);
        self.inner.stream(ctx).await
    }
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
}

fn counting(script: Vec<MockResponse>) -> (Counting, Arc<AtomicU32>, Arc<AtomicU32>) {
    let direct = Arc::new(AtomicU32::new(0));
    let streams = Arc::new(AtomicU32::new(0));
    (
        Counting {
            inner: MockModel::new().script_many(script),
            direct_complete: direct.clone(),
            streams: streams.clone(),
        },
        direct,
        streams,
    )
}

fn task() -> Task {
    Task {
        description: "go".into(),
        source: None,
        deadline: None,
    }
}

fn world() -> harness_core::World {
    default_world(std::env::temp_dir())
}

#[tokio::test]
async fn the_loop_streams_unless_told_not_to() {
    let (m, direct, streams) = counting(vec![MockResponse::text("done")]);
    assert!(AgentLoop::new(MockModel::new()).streaming);
    AgentLoop::new(m)
        .run_with_max_iters(task(), &mut world(), 3)
        .await
        .unwrap();
    assert_eq!(direct.load(Ordering::SeqCst), 0);
    assert_eq!(streams.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn opting_out_uses_complete() {
    let (m, direct, streams) = counting(vec![MockResponse::text("done")]);
    AgentLoop::new(m)
        .with_streaming(false)
        .run_with_max_iters(task(), &mut world(), 3)
        .await
        .unwrap();
    assert_eq!(direct.load(Ordering::SeqCst), 1);
    assert_eq!(streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_forced_final_synthesis_streams_too() {
    // One iteration allowed: the tool call uses it up, so the loop must force
    // a tool-free final answer, which is a second model request.
    let (m, direct, streams) = counting(vec![
        MockResponse::tool_call("read_file", json!({"path": "nope.txt"})),
        MockResponse::text("final answer"),
    ]);
    AgentLoop::new(m)
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task(), &mut world(), 1)
        .await
        .unwrap();
    assert_eq!(
        direct.load(Ordering::SeqCst),
        0,
        "no request may bypass streaming"
    );
    assert_eq!(streams.load(Ordering::SeqCst), 2);
}
