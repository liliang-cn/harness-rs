//! Manufacturing quality/process knowledge assistant — a blueprint for the SMB
//! auto-parts vertical.
//!
//! ```sh
//! cargo run -p manufacturing-quality-agent
//! ```
//!
//! Everything is local and deterministic (a scripted stand-in model + in-memory
//! knowledge) so it runs in CI with no server, network, or API key. It exercises
//! the full "on-prem quality brain" stack a real deployment ships:
//!
//! - **数据不出内网** — the model is a `ModelRouter` with `keep_local` set, so a
//!   request touching drawings/process data can never fall back to the cloud.
//! - **受控引用** — `quality_doc_search` returns only the *current controlled*
//!   revision; the obsolete Rev.B is reported but never cited (IATF 16949).
//! - **图谱** — `quality_graph` answers the relationship question (part → process
//!   → defect → historical 8D) that plain RAG cannot.
//! - **可审计** — every step lands in a hash-chained audit trail; PII is redacted;
//!   `verify_chain` proves the log wasn't tampered with.
//!
//! The cross-cutting runtime (tool tracing, hash-chained audit, identity
//! metadata) is shared via `vertical_common`; this file is just the
//! manufacturing domain.

mod knowledge;
mod tools;

use harness_context::default_world;
use harness_core::{Model, Task};
use harness_hooks::AuditHook;
use harness_loop::{AgentLoop, Outcome};
use harness_models::{KEEP_LOCAL_KEY, MockModel, MockResponse, ModelRouter};
use harness_redact::Redactor;
use serde_json::json;
use std::sync::Arc;
use tools::{QualityDocSearch, QualityGraph};
use vertical_common::{PrintToolHook, open_audit, print_audit_and_verify, request_metadata};

#[tokio::main]
async fn main() {
    let question = "图号 BRK-2049 前刹车片的摩擦系数检验标准是多少?历史上有没有相关不良?";
    println!("== 汽车零部件 质量/工艺知识助手 (本地单机) ==");
    println!("提问 (alice@质量部): {question}\n");

    // The local model drives the tools; the "cloud" leg exists only to prove
    // keep_local pins us to local — if it were ever used, no tools would run.
    let local: Arc<dyn Model> = Arc::new(
        MockModel::new()
            .with_name("local-qwen")
            .script(MockResponse::tool_call(
                "quality_doc_search",
                json!({ "query": "摩擦系数", "part_no": "BRK-2049" }),
            ))
            .script(MockResponse::tool_call(
                "quality_graph",
                json!({ "node": "BRK-2049" }),
            ))
            .script(MockResponse::text(
                "依据现行受控检验标准 QIS-BRK-2049 Rev.C:摩擦系数 0.38–0.42,试验温度 100±5℃,\
                 每批抽检 5 片并做 SPC。历史相关不良见 8D-2024-017(烧结炉温偏低导致摩擦系数偏低,\
                 已上调炉温并加 SPC 监控)。\n引用:QIS-BRK-2049 Rev.C(现行受控)。",
            )),
    );
    let cloud: Arc<dyn Model> = Arc::new(MockModel::new().with_name("cloud").script(
        MockResponse::text("【云端】本例不应被调用——数据 keep_local 锁在内网"),
    ));
    let model = ModelRouter::new(local).with_fallback(cloud);

    let (sink, audit_path) = open_audit("mqa");
    let agent = AgentLoop::new(model)
        .with_tool(Arc::new(QualityDocSearch))
        .with_tool(Arc::new(QualityGraph))
        .with_hook(Arc::new(
            AuditHook::new(sink).with_redactor(Redactor::new()),
        ))
        .with_hook(Arc::new(PrintToolHook));

    // keep_local so the request (drawings/process data) stays on the local model.
    let mut metadata = request_metadata("alice@质量部", "qc-s1", "req-mfg-1");
    metadata.insert(KEEP_LOCAL_KEY.into(), true.into());
    println!("[router] keep_local=true → 本地模型(图纸/工艺数据不出内网)\n");

    let ws = std::env::temp_dir().join(format!("mqa-ws-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);
    let task = Task {
        description: question.into(),
        source: None,
        deadline: None,
    };

    let outcome = agent
        .run_with_seed_and_metadata(task, Vec::new(), metadata, &mut world, 8)
        .await
        .expect("run");

    if let Outcome::Done { text, .. } = &outcome {
        println!("\n[answer]\n{}\n", text.clone().unwrap_or_default());
    }

    print_audit_and_verify(&audit_path);
    let _ = std::fs::remove_dir_all(&ws);

    println!(
        "\n要点:受控版本(Rev.C)可引用、过期版(Rev.B)被排除;图谱给出\
         『零件→工序→不良→8D』经验;全程本地 + 审计防篡改。\n\
         生产化:把 MockModel 换成本地 Ollama,把两个工具体换成 CortexDB 调用即可。"
    );
}
