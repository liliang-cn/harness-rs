//! The gate the agent cannot edit its way through.
//!
//! Every test here runs the real loop against a model that behaves the way the
//! failure actually shows up in practice: it cannot satisfy the check, so it
//! rewrites the check and reports success. Asserting on `SealSet` alone would
//! not prove the loop refuses the pass, which is the part that matters.

use async_trait::async_trait;
use harness_context::default_world;
use harness_core::{
    Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, Task, ToolCall, ToolError,
    ToolResult, ToolRisk, ToolSchema, World,
};
use harness_core::{Event, Hook, HookOutcome};
use harness_loop::{Acceptance, AgentLoop, Outcome, Verdict};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// The contract: this file must contain exactly `42`.
///
/// Deliberately data-driven rather than hard-coded, because a contract kept in
/// a file is the one an agent with filesystem tools can reach — which is the
/// situation being defended against.
struct ExpectFortyTwo {
    contract: PathBuf,
}

#[async_trait]
impl Acceptance for ExpectFortyTwo {
    fn name(&self) -> &str {
        "expect-42"
    }
    fn seals(&self) -> Vec<PathBuf> {
        vec![self.contract.clone()]
    }
    async fn check(&self, _ctx: &Context, world: &World) -> Verdict {
        let want = std::fs::read_to_string(world.repo.root.join(&self.contract))
            .unwrap_or_default()
            .trim()
            .to_string();
        let got = std::fs::read_to_string(world.repo.root.join("answer.txt"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if !want.is_empty() && want == got {
            Verdict::passed()
        } else {
            Verdict::failed(format!("answer.txt must contain {want:?}, found {got:?}"))
        }
    }
}

/// Writes whatever it is told, wherever it is told.
struct WriteFile;

#[async_trait]
impl harness_core::Tool for WriteFile {
    fn name(&self) -> &str {
        "write"
    }
    fn schema(&self) -> &ToolSchema {
        static S: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        S.get_or_init(|| ToolSchema {
            name: "write".into(),
            description: "Write text to a path, or delete it with text=\\0rm.".into(),
            input: serde_json::json!({
                "type": "object",
                "properties": { "path": {"type": "string"}, "text": {"type": "string"} },
                "required": ["path", "text"]
            }),
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Destructive
    }
    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let p = args["path"].as_str().unwrap_or_default();
        let t = args["text"].as_str().unwrap_or_default();
        if t == "\0rm" {
            std::fs::remove_file(world.repo.root.join(p))
                .map_err(|e| ToolError::Exec(format!("rm {p}: {e}")))?;
        } else {
            std::fs::write(world.repo.root.join(p), t)
                .map_err(|e| ToolError::Exec(format!("write {p}: {e}")))?;
        }
        Ok(ToolResult {
            ok: true,
            content: serde_json::json!({ "wrote": p }),
            trace: None,
        })
    }
}

/// A model that plays a fixed sequence of moves, then stops.
struct Scripted {
    moves: Vec<(&'static str, &'static str)>,
    at: AtomicU32,
}

impl Scripted {
    fn new(moves: Vec<(&'static str, &'static str)>) -> Self {
        Self {
            moves,
            at: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl Model for Scripted {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle: "scripted".into(),
            provider: "test".into(),
            model: "scripted".into(),
            context_window: 8192,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: true,
            supports_streaming: false,
            supports_web_grounding: false,
        }
    }
    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        let i = self.at.fetch_add(1, Ordering::SeqCst) as usize;
        match self.moves.get(i) {
            Some((path, text)) => Ok(ModelOutput {
                text: Some("working".into()),
                tool_calls: vec![ToolCall {
                    id: format!("c{i}"),
                    name: "write".into(),
                    args: serde_json::json!({ "path": path, "text": text }),
                }],
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            }),
            None => Ok(ModelOutput {
                text: Some("done".into()),
                ..Default::default()
            }),
        }
    }
}

fn workspace(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("harness-sealed-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("contract.txt"), "42").unwrap();
    d
}

fn run(dir: &PathBuf, moves: Vec<(&'static str, &'static str)>) -> Outcome {
    let loop_ = AgentLoop::new(Scripted::new(moves))
        .with_tool(Arc::new(WriteFile))
        .with_acceptance_set(vec![Arc::new(ExpectFortyTwo {
            contract: PathBuf::from("contract.txt"),
        })])
        .with_acceptance_retries(3);
    let mut world = default_world(dir);
    let task = Task {
        description: "make answer.txt satisfy the contract".into(),
        source: None,
        deadline: None,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run(task, &mut world))
        .unwrap()
}

#[test]
fn doing_the_work_passes() {
    let d = workspace("honest");
    let out = run(&d, vec![("answer.txt", "42")]);
    match out {
        Outcome::Done {
            verified,
            seal_breach,
            ..
        } => {
            assert!(verified.as_ref().is_some_and(|v| v.passed), "{verified:?}");
            assert!(seal_breach.is_none());
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn editing_the_contract_to_match_a_wrong_answer_is_refused() {
    // The whole point. The model cannot produce 42, so it rewrites the
    // contract to demand the answer it *did* produce. Every check now passes
    // on its own terms, and the run must still fail.
    let d = workspace("forged");
    let out = run(&d, vec![("answer.txt", "wrong"), ("contract.txt", "wrong")]);
    match out {
        Outcome::Done {
            verified,
            seal_breach,
            ..
        } => {
            assert!(
                !verified.as_ref().is_some_and(|v| v.passed),
                "a forged contract must not yield a pass: {verified:?}"
            );
            let breach = seal_breach.expect("the breach must be reported, not just the failure");
            assert!(breach.contains("contract.txt"), "{breach}");
            assert!(breach.contains("was modified"), "{breach}");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn deleting_the_contract_is_refused_too() {
    // `rm` must not be the cheap way through. The check here waves everything
    // past, so the seal is the only thing that can fail the run — and the
    // deletion happens through a tool call mid-run, which is how it would.
    let d = workspace("deleted");
    let loop_ = AgentLoop::new(Scripted::new(vec![("contract.txt", "\0rm")]))
        .with_tool(Arc::new(WriteFile))
        .with_acceptance_set(vec![Arc::new(AlwaysPasses {
            contract: PathBuf::from("contract.txt"),
        })])
        .with_acceptance_retries(0);
    let mut world = default_world(&d);
    let task = Task {
        description: "anything".into(),
        source: None,
        deadline: None,
    };
    let out = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run(task, &mut world))
        .unwrap();
    match out {
        Outcome::Done {
            verified,
            seal_breach,
            ..
        } => {
            let b = seal_breach.expect("deletion must breach the seal");
            assert!(b.contains("was deleted"), "{b}");
            assert!(
                !verified.as_ref().is_some_and(|v| v.passed),
                "a check that passes must still not carry a breached run"
            );
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// A check that would wave anything through — so the only thing that can fail
/// the run is the seal.
struct AlwaysPasses {
    contract: PathBuf,
}

#[async_trait]
impl Acceptance for AlwaysPasses {
    fn name(&self) -> &str {
        "always"
    }
    fn seals(&self) -> Vec<PathBuf> {
        vec![self.contract.clone()]
    }
    async fn check(&self, _c: &Context, _w: &World) -> Verdict {
        Verdict::passed()
    }
}

#[test]
fn a_check_that_seals_nothing_behaves_exactly_as_before() {
    // Sealing is opt-in; the default path must not change. A run that rewrites
    // its own tests on purpose is legitimate and must still pass.
    struct Unsealed;
    #[async_trait]
    impl Acceptance for Unsealed {
        fn name(&self) -> &str {
            "unsealed"
        }
        async fn check(&self, _c: &Context, _w: &World) -> Verdict {
            Verdict::passed()
        }
    }
    let d = workspace("unsealed");
    let loop_ = AgentLoop::new(Scripted::new(vec![("contract.txt", "rewritten")]))
        .with_tool(Arc::new(WriteFile))
        .with_acceptance_set(vec![Arc::new(Unsealed)]);
    let mut world = default_world(&d);
    let task = Task {
        description: "rewrite the contract, legitimately".into(),
        source: None,
        deadline: None,
    };
    let out = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run(task, &mut world))
        .unwrap();
    match out {
        Outcome::Done {
            verified,
            seal_breach,
            ..
        } => {
            assert!(verified.as_ref().is_some_and(|v| v.passed));
            assert!(seal_breach.is_none(), "nothing was sealed");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// Records the gate events a host would be watching for.
#[derive(Default)]
struct Watcher {
    seen: std::sync::Mutex<Vec<String>>,
}

impl Hook for Watcher {
    fn name(&self) -> &str {
        "watcher"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(
            ev,
            Event::AcceptanceChecked { .. } | Event::SealBreached { .. }
        )
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        let mut g = self.seen.lock().unwrap();
        match ev {
            Event::AcceptanceChecked { name, passed, .. } => {
                g.push(format!("checked:{name}:{passed}"))
            }
            Event::SealBreached { detail } => g.push(format!("breach:{detail}")),
            _ => {}
        }
        HookOutcome::Allow
    }
}

#[test]
fn the_gate_reports_to_hooks_not_only_to_the_caller() {
    // An audit trail that lists every tool the agent called and never says
    // whether anything agreed the work was done answers the wrong question.
    // Before this, no hook could see a verdict at all.
    let d = workspace("observed");
    let w = Arc::new(Watcher::default());
    let loop_ = AgentLoop::new(Scripted::new(vec![
        ("answer.txt", "wrong"),
        ("contract.txt", "wrong"),
    ]))
    .with_tool(Arc::new(WriteFile))
    .with_hook(w.clone())
    .with_acceptance_set(vec![Arc::new(ExpectFortyTwo {
        contract: PathBuf::from("contract.txt"),
    })])
    .with_acceptance_retries(0);
    let mut world = default_world(&d);
    let task = Task {
        description: "t".into(),
        source: None,
        deadline: None,
    };
    let _ = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run(task, &mut world))
        .unwrap();

    let seen = w.seen.lock().unwrap().clone();
    assert!(
        seen.iter().any(|e| e.starts_with("checked:expect-42")),
        "every verdict must be observable: {seen:?}"
    );
    assert!(
        seen.iter().any(|e| e.starts_with("breach:")),
        "the breach must reach hooks, not just the outcome: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_goal_runs_to_completion_one_phase_at_a_time() {
    // The shape a host should be able to write: a while-let, and nothing else.
    let d = workspace("goal-drive");
    let store = harness_loop::GoalStore::open(d.join(".goals")).unwrap();
    let mut goal = harness_loop::Goal::new("g", "make answer.txt say 42", 1)
        .with_phases(["write the answer"])
        .with_verify("answer.txt matches contract.txt");
    store.save(&goal).unwrap();

    let loop_ = AgentLoop::new(Scripted::new(vec![("answer.txt", "42")]))
        .with_tool(Arc::new(WriteFile))
        .with_acceptance_set(vec![Arc::new(ExpectFortyTwo {
            contract: PathBuf::from("contract.txt"),
        })]);
    let mut world = default_world(&d);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let step = rt
        .block_on(loop_.run_goal(&mut goal, &store, &mut world, 2))
        .unwrap();
    let (_, receipt) = step.expect("one phase was owed");
    assert!(receipt.passed, "{}", receipt.summary());
    assert!(goal.complete());

    // And it is complete *on disk*, not just in memory.
    assert!(store.load("g").unwrap().complete());
    // A second call has nothing to do.
    assert!(
        rt.block_on(loop_.run_goal(&mut goal, &store, &mut world, 3))
            .unwrap()
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_failed_phase_is_persisted_with_its_reason() {
    // The branch hosts forget. If this is not saved, resume retries the phase
    // as though nothing had been learned — and the run you most wanted a record
    // of is the one with no record.
    let d = workspace("goal-fail");
    let store = harness_loop::GoalStore::open(d.join(".goals")).unwrap();
    let mut goal =
        harness_loop::Goal::new("g", "make answer.txt say 42", 1).with_phases(["write the answer"]);

    let loop_ = AgentLoop::new(Scripted::new(vec![("answer.txt", "nope")]))
        .with_tool(Arc::new(WriteFile))
        .with_acceptance_set(vec![Arc::new(ExpectFortyTwo {
            contract: PathBuf::from("contract.txt"),
        })])
        .with_acceptance_retries(0);
    let mut world = default_world(&d);
    let (_, receipt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run_goal(&mut goal, &store, &mut world, 2))
        .unwrap()
        .expect("a phase was owed");

    assert!(!receipt.passed);
    let reloaded = store.load("g").unwrap();
    assert!(!reloaded.complete());
    let (_, phase) = reloaded.current().unwrap();
    assert_eq!(phase.status, harness_loop::PhaseStatus::Failed);
    assert!(phase.note.contains("FAILED"), "{}", phase.note);
    // And the next brief tells the model what went wrong last time.
    assert!(reloaded.brief().contains("did not hold"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_receipt_names_the_run_the_loop_actually_did() {
    // run_receipted exists so the task and model cannot drift from the run
    // they describe — the failure mode of making the caller restate them.
    let d = workspace("receipted");
    let loop_ = AgentLoop::new(Scripted::new(vec![("answer.txt", "42")]))
        .with_tool(Arc::new(WriteFile))
        .with_acceptance_set(vec![Arc::new(ExpectFortyTwo {
            contract: PathBuf::from("contract.txt"),
        })]);
    let mut world = default_world(&d);
    let task = Task {
        description: "the exact thing that was asked".into(),
        source: None,
        deadline: None,
    };
    let (_, r) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(loop_.run_receipted(task, &mut world, 1730000000000))
        .unwrap();

    assert_eq!(r.task, "the exact thing that was asked");
    assert_eq!(r.model, "scripted");
    assert_eq!(r.finished_ms, 1730000000000);
    assert!(r.intact() && r.passed);
    assert_eq!(
        r.contract.entries.len(),
        1,
        "the sealed contract is recorded"
    );
    let _ = std::fs::remove_dir_all(&d);
}
