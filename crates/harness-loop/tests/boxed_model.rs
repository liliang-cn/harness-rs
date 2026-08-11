//! The first line of code a new user writes.
//!
//! Every model factory in the workspace (`ApiKind::build`, a router, anything
//! kept behind a trait object) hands back an `Arc<dyn Model>`, which by design
//! does not implement `Model` — so `AgentLoop::new` rejects it with a trait-bound
//! error naming `DynModel`, a type the reader has not met yet. `AgentLoop::boxed`
//! is the path that takes it directly; this test is the README's quick start,
//! compiled.

use harness_context::default_world;
use harness_core::{Model, Task};
use harness_loop::{AgentLoop, Outcome};
use harness_models::{MockModel, MockResponse};
use std::sync::Arc;

#[tokio::test]
async fn boxed_takes_the_arc_a_factory_returns() {
    // What a factory hands back — the shape `ApiKind::build` returns.
    let model: Arc<dyn Model> = Arc::new(MockModel::new().script(MockResponse::text("done")));

    let ws = std::env::temp_dir().join(format!("boxed-model-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace");
    let mut world = default_world(&ws);

    let outcome = AgentLoop::boxed(model)
        .run(
            Task {
                description: "say done".into(),
                source: None,
                deadline: None,
            },
            &mut world,
        )
        .await
        .expect("run");

    let _ = std::fs::remove_dir_all(&ws);
    match outcome {
        Outcome::Done { text, .. } => assert_eq!(text.as_deref(), Some("done")),
        other => panic!("expected Done, got {other:?}"),
    }
}
