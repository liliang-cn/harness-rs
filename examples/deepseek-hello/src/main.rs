//! Smoke-test for the `Model` trait against the DeepSeek API.
//!
//! Usage:
//!
//! ```sh
//! DEEPSEEK_API_KEY=sk-... cargo run -p deepseek-hello
//! DEEPSEEK_API_KEY=sk-... cargo run -p deepseek-hello -- pro "What is a harness in software engineering?"
//! ```
//!
//! This binary:
//! 1. Builds a `SkillRegistry` of every `#[skill]`-decorated function below.
//! 2. Renders the agentskills.io–style catalogue.
//! 3. Sends it (as a system message) plus a user task to DeepSeek.
//! 4. Prints the response.

use anyhow::Context as _;
use harness::prelude::*;
use harness::skills::SkillRegistry;
use harness_core::{Block, Task, Turn, TurnRole};
use harness_models::{OpenAiCompat, providers};
use std::collections::BTreeMap;

/// Echo the user's input verbatim.
#[harness::skill(name = "echo", harness(kind = "computational", risk = "read-only"))]
async fn echo(_ctx: &mut Context, _w: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

/// Run `cargo fmt` on the Rust workspace. Use before committing Rust changes.
#[harness::skill(name = "format-rust", harness(kind = "computational", risk = "read-only"))]
async fn format_rust(_ctx: &mut Context, _w: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

/// Review an axum HTTP handler for security and error-handling issues.
#[harness::skill(name = "review-axum", harness(kind = "inferential", risk = "read-only"))]
async fn review_axum(_ctx: &mut Context, _w: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .context("DEEPSEEK_API_KEY env var is required")?;

    // CLI args: optional model tier ("flash" | "pro") + the user question
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let tier = if args.first().map(|s| s.as_str()) == Some("pro") {
        args.remove(0);
        "pro"
    } else if args.first().map(|s| s.as_str()) == Some("flash") {
        args.remove(0);
        "flash"
    } else {
        "flash"
    };
    let question = if args.is_empty() {
        "Given the available skills, which one would you activate to start, and why? Reply in one paragraph.".to_string()
    } else {
        args.join(" ")
    };

    // 1. Build the skill registry (auto-collects #[skill]-decorated fns)
    let registry = SkillRegistry::new().with_macro_skills()?;
    println!("Registered {} skill(s):", registry.len());
    for (name, s) in registry.iter() {
        println!("  - {name}: {}", s.manifest().description);
    }
    println!();

    // 2. Render the catalogue as system context
    let catalogue = registry.catalogue();

    // 3. Build a Context (system + catalogue as guide + task)
    let ctx = Context {
        system: vec![Block::Text(
            "You are a coding-agent harness orchestrator. Respond tersely.".into(),
        )],
        guides:  vec![Block::Text(catalogue)],
        history: vec![Turn {
            role:   TurnRole::User,
            blocks: vec![Block::Text(question.clone())],
        }],
        task: Task {
            description: question.clone(),
            source: None,
            deadline: None,
        },
        policy:   Default::default(),
        metadata: BTreeMap::new(),
        tools:    Vec::new(),
    };

    // 4. Call DeepSeek — pass model name directly, no factory layer
    let model_id = if tier == "pro" { "deepseek-v4-pro" } else { "deepseek-v4-flash" };
    let model = OpenAiCompat::with_key(providers::DEEPSEEK, model_id, api_key);
    let info = model.info();
    println!(
        "→ model: {} ({} via {}, window {} tokens)",
        info.handle, info.model, info.provider, info.context_window,
    );
    println!("→ question: {question}\n");

    let out = model.complete(&ctx).await?;
    println!("← response ({} input + {} output tokens):", out.usage.input_tokens, out.usage.output_tokens);
    println!("{}", out.text.unwrap_or_default());

    Ok(())
}
