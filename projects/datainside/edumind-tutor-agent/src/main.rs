//! EduMind AI 智能家教服务端 (基于 harness-rs + CortexDB + @ai-gui + Ollama OCR/Embedding)
//! 零 DI 依赖，专注于启发式辅导、教材知识图谱建图、错题诊断与概念可视化。

use harness_hooks::HashChainSink;
use harness_serve::{ChatService, CorsConfig, InMemorySessions, OpenAuth, http};
use std::sync::Arc;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port = env_or("PORT", "43300");
    let audit_path = env_or("AUDIT", "/tmp/edumind-audit.jsonl");

    println!("=== 🎓 EduMind 智学导师系统启动 (CortexDB + Ollama + harness-rs + @ai-gui) ===");

    // 1. 配置大模型代理 (用于启发式教学与高阶推理)
    let model_name = env_or("LLM_MODEL", "gpt-5.6-terra");
    let base_url = env_or("LLM_BASE", "https://cpa.superleo.app/v1");
    let api_key = env_or("LLM_KEY", "sk-cpa-211f4cbd146aa63f69730022ecca6420");
    let model = harness_models::ApiKind::OpenAI.build(base_url, model_name, api_key);

    // 2. 设置启发式教学 Prompt 指引
    let instruction = r#"你是 EduMind 智学导师 (EduMind AI Interactive Tutor)。
教学与辅导指导原则：
1. 采用苏格拉底提问法：引导学生逐步推导，严禁直接给出题目的完整解答或最终数值！每次只提示关键的第一步或提问关键前置概念。
2. 呈现数学/物理公式：必须使用标准的 LaTeX 语法格式（如 $x^2 - 6x + 5 = 0$ 或 $$ x = \frac{-b \pm \sqrt{b^2-4ac}}{2a} $$），前端由 @ai-gui/plugin-katex 自动渲染。
3. 展现图像与可视化：当涉及到函数图像、几何图形、概念结构图或数据统计时，必须在 Markdown 后紧跟一个 ```chart JSON 代码块（符合 ECharts 配置规范），前端由 @ai-gui/plugin-chart 自动交互渲染。
4. 本地 Ollama 增强支持：配合本地 Ollama 提供的 `glm-ocr`（OCR 图文解析）和 `qwen3-embedding`（向量语义搜索）加速教材构建与精准搜索。"#.to_string();

    let svc = ChatService::new(
        model,
        Arc::new(OpenAuth::new("student_xiaoming")),
        Arc::new(InMemorySessions::new()),
        std::env::temp_dir().join("edumind-ws"),
    )
    .with_audit(Arc::new(HashChainSink::new(&audit_path)?))
    .with_instruction(instruction);

    let app = http::router_with_cors(Arc::new(svc), &CorsConfig::permissive());
    let addr = format!("0.0.0.0:{port}");
    println!("[edumind-server] 智学家教端点启动成功: http://{addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
