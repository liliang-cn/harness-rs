//! crate-keeper — an end-to-end demo of the harness framework.
//!
//! Targets an arbitrary Rust workspace and asks DeepSeek to:
//!   1. Inspect the layout (`list_dir`)
//!   2. Read `Cargo.toml`
//!   3. Read one source file the agent picks
//!   4. Compose a short "what this project does" summary
//!   5. Write that summary to `<workspace>/HARNESS_NOTES.md`
//!
//! Tools available to the agent: `list_dir`, `read_file`, `write_file`,
//! `shell_read` (allowlisted). Sensors: `cargo check` on self-correct stage.
//!
//! ```sh
//! DEEPSEEK_API_KEY=sk-... cargo run -p crate-keeper -- /path/to/some/rust/workspace
//! ```

use clap::Parser;
use harness_context::default_world;
use harness_core::{Model, Task};
use harness_loop::{AgentLoop, Outcome, SessionRecorder};
use harness_models::OpenAiCompat;
use harness_sensors_rust::CargoCheck;
use harness_tools_fs::{ListDir, ReadFile, WriteFile};
use harness_tools_shell::ShellRead;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "crate-keeper",
    about = "Inspect a Rust workspace with a harness agent loop"
)]
struct Cli {
    /// Path to the Rust workspace to inspect.
    #[arg(default_value = ".")]
    workspace: PathBuf,

    /// Model tier — flash or pro.
    #[arg(long, default_value = "pro")]
    tier: String,

    /// Custom task description. Defaults to a "summarise this workspace" prompt.
    #[arg(long)]
    task: Option<String>,

    /// Maximum agent loop iterations.
    #[arg(long, default_value_t = 12)]
    max_iters: u32,

    /// Record a JSONL session log to this path; replayable via
    /// `harness trace <file>` or `harness_loop::replay_as_mock`.
    #[arg(long)]
    record: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| anyhow::anyhow!("DEEPSEEK_API_KEY env required"))?;

    let workspace = cli
        .workspace
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("workspace: {e}"))?;
    if !workspace.join("Cargo.toml").exists() {
        anyhow::bail!(
            "not a cargo workspace: {} (no Cargo.toml)",
            workspace.display()
        );
    }

    let task_description = cli.task.unwrap_or_else(|| {
        "You are inspecting a Rust workspace. Use the available tools to:\n\
         1. `list_dir` the root to see the layout,\n\
         2. `read_file` to read `Cargo.toml`,\n\
         3. Pick ONE source file under crates/ or src/ and read its first 60 lines,\n\
         4. Compose a concise (≤200 words) note describing what this project does, its tech stack, and one observation about its structure,\n\
         5. `write_file` that note to `HARNESS_NOTES.md` at the workspace root.\n\
         Then reply with one short sentence confirming the file was written. \
         Do NOT run shell_read unless strictly necessary."
            .to_string()
    });

    let model_id = match cli.tier.as_str() {
        "flash" => "deepseek-v4-flash",
        _ => "deepseek-v4-pro",
    };
    let model = OpenAiCompat::with_key("https://api.deepseek.com", model_id, api_key);
    let info = model.info();
    println!(
        "→ harness crate-keeper\n  workspace: {}\n  model:     {} ({}/{}) window={}",
        workspace.display(),
        info.handle,
        info.provider,
        info.model,
        info.context_window,
    );

    let mut loop_ = AgentLoop::new(model)
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(ShellRead))
        .with_sensor(Arc::new(CargoCheck::new()));
    if let Some(path) = &cli.record {
        let recorder = SessionRecorder::new(path)
            .map_err(|e| anyhow::anyhow!("create session log {}: {e}", path.display()))?;
        loop_ = loop_.with_hook(Arc::new(recorder));
        println!("  recording: {}", path.display());
    }
    println!(
        "  tools:     {} registered (max_iters={})\n",
        loop_.tools.len(),
        cli.max_iters
    );

    let mut world = default_world(&workspace);
    let task = Task {
        description: task_description,
        source: None,
        deadline: None,
    };
    let outcome = loop_
        .run_with_max_iters(task, &mut world, cli.max_iters)
        .await?;

    match outcome {
        Outcome::Done { text, iters, .. } => {
            println!("\n✓ done after {iters} iteration(s)");
            if let Some(t) = text {
                println!("\n--- final assistant message ---\n{t}");
            }
            let notes = workspace.join("HARNESS_NOTES.md");
            if notes.exists() {
                println!(
                    "\n📝 HARNESS_NOTES.md ({} bytes):",
                    std::fs::metadata(&notes).map(|m| m.len()).unwrap_or(0)
                );
                if let Ok(s) = std::fs::read_to_string(&notes) {
                    println!("---");
                    println!("{s}");
                    println!("---");
                }
            } else {
                eprintln!("\n⚠️  agent finished without writing HARNESS_NOTES.md");
            }
        }
        Outcome::BudgetExhausted { iters, .. } => {
            println!("\n✗ budget exhausted after {iters} iteration(s)");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HARNESS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .try_init();
}
