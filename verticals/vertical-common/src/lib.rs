//! Shared governance runtime for the vertical solutions (manufacturing / retail
//! / gym). It exists so each vertical carries only its *domain* logic (tools,
//! schema, prompts) and reuses one implementation of the cross-cutting wiring:
//! a tool-tracing hook, a hash-chained audit trail, and the identity metadata.
//! This is the fixed 80% that every deployment shares.

use harness_core::{Event, Hook, HookOutcome, Model, World};
use harness_hooks::{
    ACTOR_KEY, ChainedRecord, HashChainSink, REQUEST_KEY, SESSION_KEY, verify_chain,
};
use harness_models::ApiKind;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The local model driving a vertical: an Ollama OpenAI-compatible endpoint by
/// default (`LLM_MODEL`, `LLM_BASE` override). Data never leaves the machine.
/// Wrap in [`harness_core::DynModel`] to hand to `AgentLoop::new`.
pub fn local_model() -> Arc<dyn Model> {
    let name = std::env::var("LLM_MODEL").unwrap_or_else(|_| "qwen3.5:latest".to_string());
    let base =
        std::env::var("LLM_BASE").unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
    ApiKind::OpenAI.build(base, name, "ollama")
}

/// Prints each tool call's outcome (name, ok, pretty content) so a run surfaces
/// the real tool results — guard refusals, rows, redaction — as they happen.
pub struct PrintToolHook;

impl Hook for PrintToolHook {
    fn name(&self) -> &str {
        "print-tool"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PostToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::PostToolUse { action, result } = ev {
            let pretty = serde_json::to_string_pretty(&result.content).unwrap_or_default();
            println!(
                "  [{}] ok={} →\n{}",
                action.tool,
                result.ok,
                indent(&pretty)
            );
        }
        HookOutcome::Allow
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("      {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Open a fresh, tamper-evident (hash-chained) audit trail in a temp file,
/// tagged for uniqueness. Returns the sink to wire into an `AuditHook` and the
/// path to later [`print_audit_and_verify`].
pub fn open_audit(tag: &str) -> (Arc<HashChainSink>, PathBuf) {
    let path = std::env::temp_dir().join(format!("{tag}-audit-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let sink = Arc::new(HashChainSink::new(&path).expect("open audit trail"));
    (sink, path)
}

/// Print the audit trail (one line per record) and its integrity check, then
/// delete the temp file.
pub fn print_audit_and_verify(path: &Path) {
    println!("== 审计留痕 (hash 链, 防篡改) ==");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    for line in content.lines() {
        if let Ok(env) = serde_json::from_str::<ChainedRecord>(line) {
            println!(
                "  #{seq:<2} {kind:<12} actor={who} req={req}",
                seq = env.seq,
                kind = env.record.kind,
                who = env.record.actor.as_deref().unwrap_or("-"),
                req = env.record.request.as_deref().unwrap_or("-"),
            );
        }
    }
    match verify_chain(path) {
        Ok(v) if v.ok => println!("\n链完整性校验:OK (checked={})", v.checked),
        Ok(v) => println!("\n链完整性校验:FAILED at seq {:?}", v.broken_at),
        Err(e) => println!("\n链完整性校验错误:{e}"),
    }
    let _ = std::fs::remove_file(path);
}

/// Build the governed per-request metadata (actor + session + request id) that
/// the audit hook and model router read. Add extra flags (e.g.
/// `router.keep_local`) to the returned map as needed.
pub fn request_metadata(actor: &str, session: &str, request: &str) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert(ACTOR_KEY.to_string(), actor.into());
    m.insert(SESSION_KEY.to_string(), session.into());
    m.insert(REQUEST_KEY.to_string(), request.into());
    m
}
