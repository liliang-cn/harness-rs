use harness_serve::{ChatService, InMemorySessions, OpenAuth};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Testing direct ChatService invocation...");
    let model = harness_models::ApiKind::OpenAI.build(
        "https://cpa.superleo.app/v1",
        "gpt-5.6-terra",
        "sk-cpa-211f4cbd146aa63f69730022ecca6420",
    );
    let svc = ChatService::new(
        model,
        Arc::new(OpenAuth::new("student_xiaoming")),
        Arc::new(InMemorySessions::new()),
        std::env::temp_dir().join("edumind-test-ws"),
    ).with_instruction("你是 EduMind 智学导师。".into());

    let res = svc.chat(None, "session_1", "你好").await;
    println!("Result: {:?}", res);
    Ok(())
}
