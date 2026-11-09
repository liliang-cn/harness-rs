//! Trajectory rules, TOML loading and golden diffs, driven by hand-built event
//! streams (so each rule gets an exact pass and fail case) plus one real
//! `AgentLoop` run recorded to disk and judged from the JSONL.

use harness_context::default_world;
use harness_core::{Action, ModelOutput, Task, ToolCall, ToolResult, Usage};
use harness_loop::{
    AgentLoop, Rule, RuleSet, SessionEvent, SessionRecorder, Step, Trajectory, TrajectoryCheck,
};
use harness_models::{MockModel, MockResponse};
use harness_tools::fs::ReadFile;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// ── event builders ─────────────────────────────────────────────────────────

fn model(text: Option<&str>, calls: usize, input: u32, output: u32) -> SessionEvent {
    SessionEvent::PostModel {
        ts_ms: 0,
        output: ModelOutput {
            text: text.map(Into::into),
            tool_calls: (0..calls)
                .map(|i| ToolCall {
                    id: format!("m{i}"),
                    name: "x".into(),
                    args: json!({}),
                })
                .collect(),
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            },
            ..Default::default()
        },
    }
}

fn pre(id: &str, tool: &str, args: Value) -> SessionEvent {
    SessionEvent::PreTool {
        ts_ms: 0,
        action: Action {
            tool: tool.into(),
            call_id: id.into(),
            args,
        },
    }
}

fn post(id: &str, ok: bool, content: Value) -> SessionEvent {
    SessionEvent::PostTool {
        ts_ms: 0,
        call_id: id.into(),
        result: ToolResult {
            ok,
            content,
            trace: None,
        },
    }
}

/// A successful call: `PreTool` + `PostTool`.
fn call(id: &str, tool: &str, args: Value) -> Vec<SessionEvent> {
    vec![pre(id, tool, args), post(id, true, json!("ok"))]
}

fn failed(id: &str, tool: &str, args: Value) -> Vec<SessionEvent> {
    vec![
        pre(id, tool, args),
        post(id, false, json!({"error": "boom"})),
    ]
}

fn end() -> SessionEvent {
    SessionEvent::End { ts_ms: 0 }
}

fn traj(parts: Vec<Vec<SessionEvent>>) -> Trajectory {
    let events: Vec<SessionEvent> = parts.into_iter().flatten().collect();
    Trajectory::from_events(&events)
}

fn one(e: SessionEvent) -> Vec<SessionEvent> {
    vec![e]
}

// ── normalization ──────────────────────────────────────────────────────────

#[test]
fn normalizes_steps_and_totals() {
    let t = traj(vec![
        one(model(None, 2, 10, 5)),
        call("a", "read_file", json!({"path": "x"})),
        failed("b", "shell", json!({"cmd": "ls"})),
        one(pre("c", "edit_file", json!({}))), // never got a result
        one(model(Some("done"), 0, 20, 7)),
        one(end()),
    ]);
    assert_eq!((t.model_calls, t.tool_calls), (2, 3));
    assert_eq!((t.input_tokens, t.output_tokens), (30, 12));
    assert!(t.ended);
    assert_eq!(t.steps.len(), 5);
    assert!(matches!(
        t.steps[0],
        Step::Model {
            has_text: false,
            tool_calls: 2,
            ..
        }
    ));
    match &t.steps[2] {
        Step::Tool { ok, error, .. } => {
            assert!(!ok);
            assert_eq!(error.as_deref(), Some("boom"));
        }
        s => panic!("{s:?}"),
    }
    match &t.steps[3] {
        Step::Tool { ok, error, .. } => {
            assert!(!ok);
            assert_eq!(error.as_deref(), Some("no result"));
        }
        s => panic!("{s:?}"),
    }
    assert_eq!(t.tool_sequence(), ["read_file", "shell", "edit_file"]);
}

// ── rules: one pass and one fail each ──────────────────────────────────────

#[test]
fn requires() {
    let good = traj(vec![call("a", "read_file", json!({}))]);
    TrajectoryCheck::new()
        .requires("read_file")
        .check(&good)
        .assert_ok();

    // A call that failed does not count as having done the job.
    let bad = traj(vec![failed("a", "read_file", json!({}))]);
    let report = TrajectoryCheck::new().requires("read_file").check(&bad);
    assert!(!report.is_ok());
    assert_eq!(report.violations[0].rule, "requires");
}

#[test]
fn order() {
    let c = TrajectoryCheck::new().order("read_file", "edit_file");
    let good = traj(vec![
        call("a", "read_file", json!({})),
        call("b", "edit_file", json!({})),
    ]);
    c.check(&good).assert_ok();

    // `after` never called: nothing to order.
    c.check(&traj(vec![call("a", "read_file", json!({}))]))
        .assert_ok();

    let bad = traj(vec![
        call("b", "edit_file", json!({})),
        call("a", "read_file", json!({})),
    ]);
    let r = c.check(&bad);
    assert_eq!(r.violations.len(), 1);
    assert_eq!(r.violations[0].step, Some(0));

    // `after` with no `before` at all.
    assert!(
        !c.check(&traj(vec![call("b", "edit_file", json!({}))]))
            .is_ok()
    );
    // A failed `before` does not satisfy the order.
    let failed_before = traj(vec![
        failed("a", "read_file", json!({})),
        call("b", "edit_file", json!({})),
    ]);
    assert!(!c.check(&failed_before).is_ok());
}

#[test]
fn forbids_tool_and_args() {
    let t = traj(vec![
        call("a", "shell", json!({"cmd": "git status"})),
        call("b", "shell", json!({"cmd": "git add -A"})),
    ]);
    // With a pattern only the matching call is a violation.
    let r = TrajectoryCheck::new()
        .forbids("shell", Some("git add (-A|--all|\\.)"))
        .check(&t);
    assert_eq!(r.violations.len(), 1);
    assert_eq!(r.violations[0].step, Some(1));

    TrajectoryCheck::new()
        .forbids("shell", Some("rm -rf"))
        .check(&t)
        .assert_ok();
    // Without a pattern every call counts.
    assert_eq!(
        TrajectoryCheck::new()
            .forbids("shell", None)
            .check(&t)
            .violations
            .len(),
        2
    );
    TrajectoryCheck::new()
        .forbids("deploy", None)
        .check(&t)
        .assert_ok();
}

#[test]
fn args() {
    let c = TrajectoryCheck::new().args("edit_file", "path", "^src/");
    c.check(&traj(vec![call(
        "a",
        "edit_file",
        json!({"path": "src/a.rs"}),
    )]))
    .assert_ok();
    // No calls: vacuously fine.
    c.check(&traj(vec![])).assert_ok();

    let bad = traj(vec![call("a", "edit_file", json!({"path": "/etc/passwd"}))]);
    assert!(c.check(&bad).violations[0].message.contains("/etc/passwd"));
    // Missing field and non-string field both violate.
    assert!(
        !c.check(&traj(vec![call("a", "edit_file", json!({}))]))
            .is_ok()
    );
    assert!(
        !c.check(&traj(vec![call("a", "edit_file", json!({"path": 3}))]))
            .is_ok()
    );

    // JSON pointer into nested args.
    let nested = TrajectoryCheck::new().args("t", "/opts/path", "^ok$");
    nested
        .check(&traj(vec![call("a", "t", json!({"opts": {"path": "ok"}}))]))
        .assert_ok();
    assert!(
        !nested
            .check(&traj(vec![call("a", "t", json!({"opts": {"path": "no"}}))]))
            .is_ok()
    );
}

#[test]
fn max_steps() {
    let t = traj(vec![
        one(model(None, 1, 100, 50)),
        call("a", "x", json!({})),
        call("b", "x", json!({})),
        one(model(Some("hi"), 0, 100, 50)),
    ]);
    TrajectoryCheck::new()
        .max_model_calls(2)
        .max_tool_calls(2)
        .max_tokens(300)
        .check(&t)
        .assert_ok();
    assert_eq!(
        TrajectoryCheck::new()
            .max_model_calls(1)
            .check(&t)
            .violations
            .len(),
        1
    );
    assert_eq!(
        TrajectoryCheck::new()
            .max_tool_calls(1)
            .check(&t)
            .violations
            .len(),
        1
    );
    assert_eq!(
        TrajectoryCheck::new()
            .max_tokens(299)
            .check(&t)
            .violations
            .len(),
        1
    );
}

#[test]
fn no_repeated_failure() {
    let same = traj(vec![
        failed("a", "shell", json!({"cmd": "make"})),
        failed("b", "shell", json!({"cmd": "make"})),
        failed("c", "shell", json!({"cmd": "make"})),
    ]);
    let r = TrajectoryCheck::new().no_repeated_failure(3).check(&same);
    assert_eq!(r.violations.len(), 1);
    assert_eq!(r.violations[0].step, Some(2));
    TrajectoryCheck::new()
        .no_repeated_failure(4)
        .check(&same)
        .assert_ok();

    // Failures are counted per identical call (tool and args); other calls and
    // successes in between do not reset or add to the count.
    let varied = traj(vec![
        failed("a", "shell", json!({"cmd": "make"})),
        failed("b", "shell", json!({"cmd": "make all"})),
        call("c", "shell", json!({"cmd": "make"})),
        failed("d", "shell", json!({"cmd": "make"})),
    ]);
    TrajectoryCheck::new()
        .no_repeated_failure(3)
        .check(&varied)
        .assert_ok();
    assert!(
        !TrajectoryCheck::new()
            .no_repeated_failure(2)
            .check(&varied)
            .is_ok()
    );
}

#[test]
fn ends() {
    let good = traj(vec![one(model(Some("answer"), 0, 1, 1)), one(end())]);
    TrajectoryCheck::new().ends().check(&good).assert_ok();

    let no_end = traj(vec![one(model(Some("answer"), 0, 1, 1))]);
    assert!(!TrajectoryCheck::new().ends().check(&no_end).is_ok());

    let cut_off = traj(vec![
        one(model(Some("hm"), 1, 1, 1)),
        one(model(None, 1, 1, 1)),
        one(end()),
    ]);
    let r = TrajectoryCheck::new().ends().check(&cut_off);
    assert_eq!(r.violations[0].step, Some(1));

    assert!(
        !TrajectoryCheck::new()
            .ends()
            .check(&traj(vec![one(end())]))
            .is_ok()
    );
}

// ── report ─────────────────────────────────────────────────────────────────

#[test]
fn report_display_and_assert_ok() {
    let t = traj(vec![call("a", "shell", json!({"cmd": "git add -A"}))]);
    let report = TrajectoryCheck::new()
        .requires("read_file")
        .forbids("shell", Some("git add -A"))
        .check(&t);
    let text = report.to_string();
    assert_eq!(text.lines().count(), 2);
    assert!(text.contains("[requires] "));
    assert!(text.contains("[forbids] step 0: "));
    assert_eq!(
        TrajectoryCheck::new().check(&t).to_string(),
        "trajectory ok"
    );

    let panicked = std::panic::catch_unwind(|| report.assert_ok());
    let msg = *panicked.unwrap_err().downcast::<String>().unwrap();
    assert!(msg.contains("[requires]"), "{msg}");
}

// ── TOML ───────────────────────────────────────────────────────────────────

#[test]
fn rule_set_from_toml() {
    let set = RuleSet::from_toml_str(
        r#"
[[rules]]
rule = "requires"
tool = "read_file"

[[rules]]
rule = "forbids"
tool = "shell"
args_match = "git add (-A|--all|\\.)"

[[rules]]
rule = "max_steps"
tool_calls = 5

[[rules]]
rule = "no_repeated_failure"
times = 3

[[rules]]
rule = "ends"
"#,
    )
    .unwrap();
    assert_eq!(set.rules.len(), 5);
    assert_eq!(
        set.rules[1],
        Rule::Forbids {
            tool: "shell".into(),
            args_match: Some("git add (-A|--all|\\.)".into())
        }
    );

    let t = traj(vec![
        one(model(Some("done"), 0, 1, 1)),
        call("a", "shell", json!({"cmd": "git add ."})),
        one(end()),
    ]);
    let r = set.check(&t);
    let names: Vec<&str> = r.violations.iter().map(|v| v.rule.as_str()).collect();
    assert_eq!(names, ["requires", "forbids"]);
}

#[test]
fn rule_set_rejects_bad_input() {
    assert!(RuleSet::from_toml_str("[[rules]]\nrule = \"nope\"\n").is_err());
    assert!(RuleSet::from_toml_str("[[rules]]\nrule = \"requires\"\n").is_err());
    let e = RuleSet::from_toml_str(
        "[[rules]]\nrule = \"args\"\ntool = \"t\"\nfield = \"f\"\npattern = \"(\"\n",
    )
    .unwrap_err();
    assert!(e.to_string().contains("invalid regex"), "{e}");
}

// ── golden ─────────────────────────────────────────────────────────────────

fn seq(tools: &[&str]) -> Trajectory {
    traj(
        tools
            .iter()
            .enumerate()
            .map(|(i, t)| call(&format!("c{i}"), t, json!({})))
            .collect(),
    )
}

#[test]
fn golden_diff() {
    let golden = seq(&["read_file", "edit_file", "shell"]);
    let same = seq(&["read_file", "edit_file", "shell"]).compare_to_golden(&golden);
    assert_eq!((same.distance, same.ratio), (0, 0.0));
    assert!(same.within(0.0));

    // One extra call: distance 1 of 4.
    let extra = seq(&["read_file", "read_file", "edit_file", "shell"]).compare_to_golden(&golden);
    assert_eq!(
        (extra.distance, extra.golden_len, extra.actual_len),
        (1, 3, 4)
    );
    assert!((extra.ratio - 0.25).abs() < 1e-9);
    assert!(extra.within(0.3));
    assert!(!extra.within(0.2));

    // One substitution.
    assert_eq!(
        seq(&["read_file", "write_file", "shell"])
            .compare_to_golden(&golden)
            .distance,
        1
    );
    // Nothing in common.
    let far = seq(&["a", "b", "c"]).compare_to_golden(&golden);
    assert_eq!(far.ratio, 1.0);
    // Both empty is no drift.
    assert_eq!(seq(&[]).compare_to_golden(&seq(&[])).ratio, 0.0);
    // Model text and args are ignored: only tool names count.
    let other_args = traj(vec![call("a", "read_file", json!({"path": "different"}))]);
    assert_eq!(
        other_args.compare_to_golden(&seq(&["read_file"])).distance,
        0
    );
}

// ── end to end: a real loop, recorded, loaded, judged ──────────────────────

struct TestDir(PathBuf);
static TD_SEQ: AtomicU64 = AtomicU64::new(0);
impl TestDir {
    fn new() -> Self {
        let n = TD_SEQ.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "harness-trajectory-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn judges_a_recorded_agent_loop_run() {
    let td = TestDir::new();
    std::fs::write(td.0.join("notes.txt"), "hello\n").unwrap();
    let log = td.0.join("session.jsonl");

    let model = MockModel::new()
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "notes.txt"}),
        ))
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "missing.txt"}),
        ))
        .script(MockResponse::text("all done"));
    let mut world = default_world(td.0.clone());
    AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_hook(Arc::new(SessionRecorder::new(&log).unwrap()))
        .run_with_max_iters(
            Task {
                description: "read both".into(),
                source: None,
                deadline: None,
            },
            &mut world,
            6,
        )
        .await
        .unwrap();

    let t = Trajectory::load(&log).unwrap();
    assert_eq!(t.tool_sequence(), ["read_file", "read_file"]);
    assert_eq!(t.model_calls, 3);

    TrajectoryCheck::new()
        .requires("read_file")
        .forbids("shell", None)
        .args("read_file", "path", "\\.txt$")
        .max_tool_calls(5)
        .no_repeated_failure(2)
        .ends()
        .check(&t)
        .assert_ok();

    // The second read failed once, so a limit of one failure trips.
    let r = TrajectoryCheck::new().no_repeated_failure(1).check(&t);
    assert_eq!(r.violations.len(), 1);
    // A golden run with the same shape has no drift.
    assert!(
        t.compare_to_golden(&seq(&["read_file", "read_file"]))
            .within(0.0)
    );
}
