//! `harness-daemon` — long-lived process that runs declarative jobs.
//!
//! ```sh
//! harness-daemon --config ~/.config/harness/daemon.toml         # run forever
//! harness-daemon --config ./daemon.toml --dry-run               # print next fires + exit
//! harness-daemon --config ./daemon.toml --once <job-name>       # fire one job now + exit
//! ```

use clap::Parser;
use harness_daemon::{Daemon, DaemonConfig};

#[derive(Parser, Debug)]
#[command(
    name = "harness-daemon",
    about = "Scheduled-job runner for the harness-rs ecosystem."
)]
struct Cli {
    /// Path to the TOML config (default: ~/.config/harness/daemon.toml).
    #[arg(long, short)]
    config: Option<std::path::PathBuf>,

    /// Print every job's next scheduled fire time and exit. No jobs executed.
    #[arg(long)]
    dry_run: bool,

    /// Run one named job RIGHT NOW (synchronously, with the configured argv +
    /// env), wait for it to finish, then exit. Use for ad-hoc invocations
    /// without bypassing the daemon's env/argv config.
    #[arg(long, value_name = "JOB_NAME")]
    once: Option<String>,
}

fn default_config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("harness")
        .join("daemon.toml")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HARNESS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .try_init();

    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(default_config_path);

    let src = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let cfg: DaemonConfig =
        toml::from_str(&src).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
    let daemon = Daemon::from_config(cfg)?;

    if cli.dry_run {
        println!("config: {}", path.display());
        daemon.dry_run();
        return Ok(());
    }

    if let Some(job_name) = cli.once {
        let Some(job) = daemon.jobs.iter().find(|j| j.name == job_name) else {
            anyhow::bail!("no job named `{job_name}` (or it's disabled)");
        };
        tracing::info!(job = %job.name, "running once: {}", job.argv.join(" "));
        let status = tokio::process::Command::new(&job.argv[0])
            .args(&job.argv[1..])
            .envs(&job.env)
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!("job exited with {:?}", status.code());
        }
        return Ok(());
    }

    tracing::info!(jobs = daemon.jobs.len(), config = %path.display(), "harness-daemon starting");
    daemon.run().await?;
    Ok(())
}
