//! Record an AgentLoop run to JSONL, then replay it through MockModel and
//! assert the outcome is bit-identical. This is the v0.2 "session replay"
//! deliverable per DESIGN.md §15.

use harness_context::default_world;
use harness_core::{Block, Task, Turn, TurnRole};
use harness_loop::{
    AgentLoop, Outcome, SessionEvent, SessionRecorder, SessionStats, read_session,
};
use harness_models::{MockModel, MockResponse};
use harness_tools_fs::ReadFile;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
        let p = std::env::temp_dir().join(format!("harness-replay-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn task(desc: &str) -> Task {
    Task { description: desc.into(), source: None, deadline: None }
}

#[tokio::test]
async fn record_captures_full_lifecycle() {
    let td = TestDir::new();
    std::fs::write(td.0.join("input.txt"), "hello world\n").unwrap();
    let log = td.0.join("session.jsonl");

    let recorder = Arc::new(SessionRecorder::new(&log).unwrap());
    let model = MockModel::new()
        .script(MockResponse::tool_call("read_file", json!({"path": "input.txt"})))
        .script(MockResponse::text("file says hello"));

    let mut world = default_world(td.0.clone());
    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_hook(recorder)
        .run_with_max_iters(task("read the file"), &mut world, 5)
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Done { .. }));

    let events = read_session(&log).unwrap();

    // Must have: SessionStart, at least two PostModel, at least one PreTool +
    // matching PostTool, SessionEnd.
    let model_calls = events.iter().filter(|e| matches!(e, SessionEvent::PostModel { .. })).count();
    let pre_tools = events.iter().filter(|e| matches!(e, SessionEvent::PreTool { .. })).count();
    let post_tools = events.iter().filter(|e| matches!(e, SessionEvent::PostTool { .. })).count();
    assert!(events.iter().any(|e| matches!(e, SessionEvent::Start { .. })));
    assert!(events.iter().any(|e| matches!(e, SessionEvent::End { .. })));
    assert_eq!(model_calls, 2, "expected 2 model calls");
    assert_eq!(pre_tools, 1, "expected 1 pre-tool event");
    assert_eq!(post_tools, 1, "expected 1 post-tool event");
}

#[tokio::test]
async fn replay_reproduces_original_outcome() {
    let td = TestDir::new();
    std::fs::write(td.0.join("greeting.txt"), "hi\n").unwrap();
    let log = td.0.join("session.jsonl");

    // ---------- record ----------
    let recorder = Arc::new(SessionRecorder::new(&log).unwrap());
    let original_model = MockModel::new()
        .script(MockResponse::tool_call("read_file", json!({"path": "greeting.txt"})))
        .script(MockResponse::text("greeting recorded"));

    let mut world1 = default_world(td.0.clone());
    let original = AgentLoop::new(original_model)
        .with_tool(Arc::new(ReadFile))
        .with_hook(recorder)
        .run_with_max_iters(task("read it"), &mut world1, 5)
        .await
        .unwrap();
    let (orig_text, orig_iters) = match &original {
        Outcome::Done { text, iters, .. } => (text.clone(), *iters),
        other => panic!("expected Done, got {other:?}"),
    };

    // ---------- replay ----------
    let events = read_session(&log).unwrap();
    let replay_model = harness_loop::replay::replay_as_mock_via_events(&events);

    let mut world2 = default_world(td.0.clone());
    let replayed = AgentLoop::new(replay_model)
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task("read it"), &mut world2, 5)
        .await
        .unwrap();

    match replayed {
        Outcome::Done { text, iters, .. } => {
            assert_eq!(text, orig_text, "replay diverged on final text");
            assert_eq!(iters, orig_iters, "replay diverged on iteration count");
        }
        other => panic!("replay expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn stats_summarise_a_real_run() {
    let td = TestDir::new();
    std::fs::write(td.0.join("a.txt"), "a\n").unwrap();
    let log = td.0.join("session.jsonl");
    let recorder = Arc::new(SessionRecorder::new(&log).unwrap());

    let model = MockModel::new()
        .script(MockResponse::tool_call("read_file", json!({"path": "a.txt"})).with_usage(100, 20))
        .script(MockResponse::tool_call("read_file", json!({"path": "a.txt"})).with_usage(120, 25))
        .script(MockResponse::text("done").with_usage(150, 30));

    let mut world = default_world(td.0.clone());
    AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_hook(recorder)
        .run_with_max_iters(task("anything"), &mut world, 5)
        .await
        .unwrap();

    let events = read_session(&log).unwrap();
    let s = SessionStats::from(&events);
    assert_eq!(s.model_calls, 3);
    assert_eq!(s.tool_calls,  2);
    assert!(s.events >= 9, "expected at least 9 events, got {}", s.events);
    assert_eq!(s.input_tokens,  100 + 120 + 150);
    assert_eq!(s.output_tokens, 20  + 25  + 30);
}

#[tokio::test]
async fn corrupted_log_lines_are_skipped_not_panicked() {
    let td = TestDir::new();
    let log = td.0.join("bad.jsonl");
    let mut content = String::new();
    content.push_str(&serde_json::to_string(&SessionEvent::Start { ts_ms: 0, source: "Startup".into() }).unwrap());
    content.push('\n');
    content.push_str("{this is not valid json\n");
    content.push_str("\n");
    content.push_str(&serde_json::to_string(&SessionEvent::End { ts_ms: 100 }).unwrap());
    content.push('\n');
    std::fs::write(&log, content).unwrap();

    let events = read_session(&log).unwrap();
    // 2 valid lines + 1 garbage + 1 empty = 2 valid events
    assert_eq!(events.len(), 2);
}

// Silence an unused-import warning when the test build path doesn't reach
// `Block`/`Turn`/`TurnRole`.
fn _silence_unused() {
    let _ = (Block::Text("".into()), TurnRole::User);
    let _t = Turn { role: TurnRole::User, blocks: vec![] };
}
