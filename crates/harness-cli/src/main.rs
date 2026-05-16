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
    }
}

fn tracing_subscriber_init() {
    // intentionally minimal; replace with tracing-subscriber later
    let _ = tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default());
}
