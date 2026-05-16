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
    }
}

fn tracing_subscriber_init() {
    // intentionally minimal; replace with tracing-subscriber later
    let _ = tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default());
}
