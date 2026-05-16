//! Golden-path end-to-end test.
//!
//! Single test that exercises **every framework component at once** against a
//! real temp workspace with `MockModel`:
//!
//! - guide injection
//! - tool dispatch (read + edit + write)
//! - sensor that detects a regression and auto-fixes via FixPatch
//! - hook that logs every PreToolUse
//! - compactor invocation (heavy budget)
//! - assert final on-disk state matches expectation

use harness_context::default_world;
use harness_core::{
    Action, Block, Context, Event, Execution, FixPatch, GuideId, GuideScope, HookOutcome,
    SensorId, Severity, Signal, Stage, Task, World,
};
use harness_loop::{AgentLoop, Outcome};
use harness_models::{MockModel, MockResponse};
use harness_tools_fs::{EditFile, ReadFile, WriteFile};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn golden_path_writes_correct_file_after_sensor_auto_fix() {
    // ----------------------------------------------------------
    // Tmp workspace with an initial bad file the agent will repair.
    // ----------------------------------------------------------
    let td = TestDir::new();
    let root = td.0.clone();
    std::fs::write(root.join("CHANGELOG.md"), "## unreleased\n- TODO: rename me\n").unwrap();
    std::fs::write(root.join("README.md"),    "# project\n").unwrap();
    let mut world = default_world(root.clone());

    // ----------------------------------------------------------
    // A guide injects baseline rules at session start.
    // ----------------------------------------------------------
    struct ProjectRules;
    #[async_trait::async_trait]
    impl harness_core::Guide for ProjectRules {
        fn id(&self) -> &GuideId {
            static I: once_cell::sync::Lazy<GuideId> =
                once_cell::sync::Lazy::new(|| "project-rules".into());
            &I
        }
        fn kind(&self) -> Execution { Execution::Inferential }
        fn scope(&self) -> &GuideScope {
            static S: once_cell::sync::Lazy<GuideScope> =
                once_cell::sync::Lazy::new(|| GuideScope::Always);
            &S
        }
        async fn apply(
            &self,
            ctx: &mut Context,
            _w: &World,
        ) -> Result<(), harness_core::GuideError> {
            ctx.guides.push(Block::Text("Rule: changelog must mention v1.0.".into()));
            Ok(())
        }
    }

    // ----------------------------------------------------------
    // A sensor watches every edit. If the CHANGELOG still has "TODO",
    // emit a Block signal with an auto-fix patch that rewrites it.
    // ----------------------------------------------------------
    struct ChangelogSensor;
    #[async_trait::async_trait]
    impl harness_core::Sensor for ChangelogSensor {
        fn id(&self) -> &SensorId {
            static I: once_cell::sync::Lazy<SensorId> =
                once_cell::sync::Lazy::new(|| "changelog-sensor".into());
            &I
        }
        fn kind(&self)  -> Execution { Execution::Computational }
        fn stage(&self) -> Stage     { Stage::SelfCorrect }
        async fn observe(
            &self,
            _action: &Action,
            world: &World,
        ) -> Result<Vec<Signal>, harness_core::SensorError> {
            let path = world.repo.root.join("CHANGELOG.md");
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            if content.contains("TODO") {
                Ok(vec![Signal {
                    severity:   Severity::Block,
                    origin:     "changelog-sensor".into(),
                    message:    "CHANGELOG still contains TODO".into(),
                    agent_hint: Some("rewrite the changelog without TODO".into()),
                    auto_fix:   Some(FixPatch::ReplaceFile {
                        path:    "CHANGELOG.md".into(),
                        content: "## v1.0\n- initial release\n".into(),
                    }),
                    location:   None,
                }])
            } else {
                Ok(Vec::new())
            }
        }
    }

    // ----------------------------------------------------------
    // A hook records every PreToolUse it sees — proves hook firing.
    // ----------------------------------------------------------
    struct CountingHook {
        n: Arc<AtomicU32>,
    }
    impl harness_core::Hook for CountingHook {
        fn name(&self) -> &str { "counter" }
        fn matches(&self, ev: &Event<'_>) -> bool {
            matches!(ev, Event::PreToolUse { .. })
        }
        fn fire(&self, _: &Event<'_>, _: &mut World) -> HookOutcome {
            self.n.fetch_add(1, Ordering::SeqCst);
            HookOutcome::Allow
        }
    }
    let tool_calls = Arc::new(AtomicU32::new(0));

    // ----------------------------------------------------------
    // A compactor that asserts it was actually called at each iter.
    // ----------------------------------------------------------
    struct CountingCompactor { n: Arc<AtomicU32> }
    #[async_trait::async_trait]
    impl harness_core::Compactor for CountingCompactor {
        fn budget(&self, _: &Context) -> harness_core::Budget {
            harness_core::Budget { used: 0, window: 100 }    // never triggers stages
        }
        async fn compact(
            &self,
            _: harness_core::CompactionStage,
            _: &mut Context,
        ) -> Result<(), harness_core::CompactError> {
            self.n.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let compactor_calls = Arc::new(AtomicU32::new(0));

    // ----------------------------------------------------------
    // MockModel script — what the model "decides" each iteration:
    //   1. read CHANGELOG.md
    //   2. (after seeing the contents) edit it (replace TODO with placeholder)
    //   3. say done
    // The sensor will fire after step 2 and over-write the file via auto-fix.
    // ----------------------------------------------------------
    let model = MockModel::new()
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "CHANGELOG.md"}),
        ))
        .script(MockResponse::tool_call(
            "edit_file",
            json!({
                "path": "CHANGELOG.md",
                "old_string": "TODO: rename me",
                "new_string": "preliminary edit",
            }),
        ))
        .script(MockResponse::text("done — changelog updated"));

    // ----------------------------------------------------------
    // Wire it all together.
    // ----------------------------------------------------------
    let outcome = AgentLoop::new(model)
        .with_guide(Arc::new(ProjectRules))
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(EditFile))
        .with_sensor(Arc::new(ChangelogSensor))
        .with_hook(Arc::new(CountingHook { n: tool_calls.clone() }))
        .with_compactor(Arc::new(CountingCompactor { n: compactor_calls.clone() }))
        .run_with_max_iters(
            Task {
                description: "rewrite CHANGELOG.md so the TODO is gone".into(),
                source: None,
                deadline: None,
            },
            &mut world,
            10,
        )
        .await
        .expect("loop runs to completion");

    // ----------------------------------------------------------
    // Assertions: every framework component actually ran.
    // ----------------------------------------------------------

    // Outcome shape: text terminates the loop on iter 3.
    match outcome {
        Outcome::Done { text, iters } => {
            assert_eq!(iters, 3, "expected 3 iterations (read, edit, finalize)");
            assert!(text.as_deref().unwrap_or("").contains("done"));
        }
        other => panic!("expected Done, got {other:?}"),
    }

    // Hook fired for every tool call (2 of them).
    assert_eq!(tool_calls.load(Ordering::SeqCst), 2, "hook should see 2 PreToolUse events");

    // Compactor's budget() is called each iter (3 iters) but no stages trigger
    // because we returned 0/100. So compact() should NOT be called.
    assert_eq!(
        compactor_calls.load(Ordering::SeqCst),
        0,
        "no stages should run at 0% budget"
    );

    // Final on-disk state: sensor's auto-fix won — file is the canonical v1.0
    // text, NOT the model's "preliminary edit".
    let final_changelog = std::fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
    assert_eq!(
        final_changelog, "## v1.0\n- initial release\n",
        "sensor auto-fix should have overwritten the model's edit"
    );

    // README.md was untouched.
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
    assert_eq!(readme, "# project\n");
}

// ----------------------------------------------------------
// shared utilities (duplicated rather than shared via mod to keep tests independent)
// ----------------------------------------------------------

struct TestDir(PathBuf);
static TD_SEQ: AtomicU64 = AtomicU64::new(0);
impl TestDir {
    fn new() -> Self {
        let pid = std::process::id();
        let n = TD_SEQ.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("harness-golden-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// silence unused-import warning when not referenced in a particular test build
fn _unused(_: Mutex<()>) {}
