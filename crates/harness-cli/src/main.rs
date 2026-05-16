use clap::{Parser, Subcommand};
use harness_core::Skill;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "harness", version, about = "Harness agent framework CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Scaffold a new agent project that depends on `harness` and demonstrates
    /// one `#[skill]`, one `#[tool]`, and a minimal `AgentLoop` in `main`.
    New {
        /// Directory name (and cargo package name) of the new project.
        name: String,
        /// Parent directory (defaults to the current dir).
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum SkillsCmd {
    Validate {
        path: PathBuf,
    },
    List {
        dir: PathBuf,
    },
    /// Export every registered skill (filesystem + #[skill] macro) to a
    /// spec-compliant directory tree that Claude Code / Cursor / Codex can
    /// consume.
    Export {
        /// Target directory; created if missing.
        target: PathBuf,
        /// Pull skills from this filesystem directory too (optional).
        #[arg(long)]
        from: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Skills { cmd: SkillsCmd::Validate { path } } => {
            match harness_skills::load_skill_dir(&path) {
                Ok(s) => {
                    println!("✓ valid: {} — {}", s.manifest().name, s.manifest().description);
                    Ok(())
                }
                Err(e) => {
                    eprintln!("✗ invalid: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Skills { cmd: SkillsCmd::List { dir } } => {
            let skills = harness_skills::scan_skills_root(&dir)?;
            for s in &skills {
                println!("{}  —  {}", s.manifest().name, s.manifest().description);
            }
            println!("\n{} skill(s)", skills.len());
            Ok(())
        }
        Cmd::Skills { cmd: SkillsCmd::Export { target, from } } => {
            let mut registry = harness_skills::SkillRegistry::new()
                .with_macro_skills()?;
            if let Some(p) = from {
                registry = registry.with_filesystem_root(&p)?;
            }
            let paths = harness_skills::export_registry(&registry, &target)?;
            for p in &paths {
                println!("✓ {}", p.display());
            }
            println!("\nexported {} skill(s) to {}", paths.len(), target.display());
            Ok(())
        }
        Cmd::New { name, path } => scaffold_new_project(name, path),
    }
}

fn scaffold_new_project(name: String, parent: Option<PathBuf>) -> anyhow::Result<()> {
    // Validate name as a cargo package identifier — same rules as agentskills.io
    // (lowercase, hyphens, no leading/trailing hyphen) plus Rust's no-leading-digit.
    if name.is_empty() {
        anyhow::bail!("project name must not be empty");
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        anyhow::bail!("project name must not start with a digit");
    }
    if name.starts_with('-') || name.ends_with('-') {
        anyhow::bail!("project name must not start or end with `-`");
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            anyhow::bail!("project name contains invalid char `{c}` — use [a-z0-9_-]");
        }
    }

    let root = parent.unwrap_or_else(|| std::env::current_dir().unwrap());
    let dir = root.join(&name);
    if dir.exists() {
        anyhow::bail!("`{}` already exists", dir.display());
    }
    std::fs::create_dir_all(dir.join("src"))?;

    let cargo_toml = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
description = "An agent built with the harness framework."

[[bin]]
name = "{name}"
path = "src/main.rs"

[dependencies]
harness        = "0.0.1"
harness-core   = "0.0.1"
harness-loop   = "0.0.1"
harness-models = "0.0.1"
harness-tools-fs = "0.0.1"
harness-context = "0.0.1"
tokio          = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
anyhow         = "1"
serde_json     = "1"
"#
    );
    std::fs::write(dir.join("Cargo.toml"), cargo_toml)?;

    let main_rs = r###"//! Minimal harness agent. Reads a file the model asks for, then summarises.

use harness::prelude::*;
use harness_context::default_world;
use harness_loop::AgentLoop;
use harness_models::{OpenAiCompat, providers};
use harness_tools_fs::{ListDir, ReadFile};
use std::sync::Arc;

/// Echo any text the user provides. Use when the user asks the agent to repeat something verbatim.
#[harness::skill(
    name = "echo",
    harness(kind = "computational", risk = "read-only"),
)]
async fn echo(_ctx: &mut Context, _w: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

/// Reverse a string. Demonstrates a #[tool] with explicit JSON schema.
#[harness::tool(
    name = "reverse",
    risk = "read-only",
    schema = r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#,
)]
async fn reverse(
    args: serde_json::Value,
    _w: &mut harness::World,
) -> Result<harness::ToolResult, harness::ToolError> {
    let t = args["text"].as_str().unwrap_or("");
    Ok(harness::ToolResult {
        ok: true,
        content: serde_json::json!({ "reversed": t.chars().rev().collect::<String>() }),
        trace: None,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| anyhow::anyhow!("set DEEPSEEK_API_KEY first"))?;
    let model = OpenAiCompat::new(providers::deepseek_flash(key));
    let mut world = default_world(".");

    let task = Task {
        description: "List the top-level files here, then describe what you find in one sentence.".into(),
        source: None,
        deadline: None,
    };

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task, &mut world, 6)
        .await?;

    println!("{outcome:?}");
    Ok(())
}
"###;
    std::fs::write(dir.join("src/main.rs"), main_rs)?;

    println!("✓ created {}/", dir.display());
    println!("  └─ Cargo.toml");
    println!("  └─ src/main.rs   (one #[skill], one #[tool], a minimal AgentLoop)");
    println!();
    println!("Next steps:");
    println!("  cd {}", dir.display());
    println!("  export DEEPSEEK_API_KEY=…");
    println!("  cargo run");
    Ok(())
}

fn tracing_subscriber_init() {
    // intentionally minimal; replace with tracing-subscriber later
    let _ = tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default());
}
