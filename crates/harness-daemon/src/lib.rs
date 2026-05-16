//! Optional standalone scheduler for the harness-rs ecosystem.
//!
//! This crate is **not** depended on by any other harness-rs-* crate; you install
//! it only if you want background scheduled jobs. It's a thin wrapper around
//! `tokio` time + a TOML config that lets you say "run *this shell command* on
//! *this schedule*" — typically pointing at a harness-rs-built agent binary
//! like `assistant --brief`.
//!
//! Design split:
//! - **Agent binary** = single-shot or REPL, runs once per invocation. No
//!   daemon in its address space.
//! - **`harness-daemon`** = long-lived process that wakes up on schedule and
//!   spawns the agent binary as a subprocess. Optional. Crashes / restarts
//!   independently of the agent.
//!
//! Use the included `harness-daemon` binary, or build your own daemon by
//! constructing a [`Daemon`] in code.

#[allow(unused_imports)]
use chrono::Timelike;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Weekday};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;
use tokio::process::Command;

// =================================================================
// Config types
// =================================================================

/// Top-level config — a list of `[[job]]` entries in TOML.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonConfig {
    #[serde(default, rename = "job")]
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Human-readable name; appears in logs.
    pub name: String,
    /// When to run. See [`Schedule`].
    pub schedule: String,
    /// Shell command to execute. Parsed with whitespace split (no quoting).
    /// For arguments with spaces use `argv` instead.
    #[serde(default)]
    pub command: Option<String>,
    /// Explicit argv: `["assistant", "--brief", "--tier", "flash"]`. Wins over `command`.
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    /// Per-job environment variables. Layered on top of the daemon's own env.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory for the spawned process. Defaults to $HOME.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Job is disabled if false. Default true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}
fn default_enabled() -> bool {
    true
}

// =================================================================
// Schedule parser
// =================================================================

/// Parsed schedule expression. Three forms today:
///
/// | TOML string | Meaning |
/// |-------------|---------|
/// | `"daily 08:00"`             | once per day at 08:00 local |
/// | `"weekly mon 09:30"`        | every Monday at 09:30 local |
/// | `"every 5m"` / `"every 1h"` | fixed interval, starts now |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Daily {
        hour: u8,
        minute: u8,
    },
    Weekly {
        weekday: Weekday,
        hour: u8,
        minute: u8,
    },
    Interval(Duration),
}

#[derive(Debug, Error)]
pub enum ScheduleError {
    #[error(
        "schedule string must be 'daily HH:MM', 'weekly <day> HH:MM', or 'every Nm/h/d/s'; got: {0}"
    )]
    Format(String),
    #[error("invalid time `{0}` — use HH:MM")]
    BadTime(String),
    #[error("invalid weekday `{0}` — use mon/tue/wed/thu/fri/sat/sun")]
    BadWeekday(String),
    #[error("invalid interval `{0}` — use Ns / Nm / Nh / Nd")]
    BadInterval(String),
}

impl Schedule {
    pub fn parse(s: &str) -> Result<Self, ScheduleError> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("daily ") {
            let (h, m) = parse_hhmm(rest)?;
            return Ok(Schedule::Daily { hour: h, minute: m });
        }
        if let Some(rest) = s.strip_prefix("weekly ") {
            let mut parts = rest.split_whitespace();
            let wd_str = parts
                .next()
                .ok_or_else(|| ScheduleError::Format(s.to_string()))?;
            let time_str = parts
                .next()
                .ok_or_else(|| ScheduleError::Format(s.to_string()))?;
            let weekday = parse_weekday(wd_str)?;
            let (h, m) = parse_hhmm(time_str)?;
            return Ok(Schedule::Weekly {
                weekday,
                hour: h,
                minute: m,
            });
        }
        if let Some(rest) = s.strip_prefix("every ") {
            return parse_interval(rest).map(Schedule::Interval);
        }
        Err(ScheduleError::Format(s.to_string()))
    }

    /// Compute the next fire time strictly after `now`.
    pub fn next_after(&self, now: DateTime<Local>) -> DateTime<Local> {
        match *self {
            Schedule::Daily { hour, minute } => {
                let today = now.date_naive();
                let t = NaiveTime::from_hms_opt(hour.into(), minute.into(), 0).unwrap();
                let candidate = Local
                    .from_local_datetime(&today.and_time(t))
                    .earliest()
                    .unwrap();
                if candidate > now {
                    candidate
                } else {
                    candidate + Duration::days(1)
                }
            }
            Schedule::Weekly {
                weekday,
                hour,
                minute,
            } => {
                // Step forward up to 7 days to find the right weekday.
                let mut candidate = now;
                for _ in 0..8 {
                    if candidate.weekday() == weekday {
                        let t = NaiveTime::from_hms_opt(hour.into(), minute.into(), 0).unwrap();
                        let attempt = Local
                            .from_local_datetime(&candidate.date_naive().and_time(t))
                            .earliest()
                            .unwrap();
                        if attempt > now {
                            return attempt;
                        }
                    }
                    candidate += Duration::days(1);
                }
                unreachable!("weekday must occur within 7 days")
            }
            Schedule::Interval(d) => now + d,
        }
    }
}

fn parse_hhmm(s: &str) -> Result<(u8, u8), ScheduleError> {
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| ScheduleError::BadTime(s.into()))?;
    let h: u8 = h.parse().map_err(|_| ScheduleError::BadTime(s.into()))?;
    let m: u8 = m.parse().map_err(|_| ScheduleError::BadTime(s.into()))?;
    if h > 23 || m > 59 {
        return Err(ScheduleError::BadTime(s.into()));
    }
    Ok((h, m))
}

fn parse_weekday(s: &str) -> Result<Weekday, ScheduleError> {
    match s.to_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        _ => Err(ScheduleError::BadWeekday(s.into())),
    }
}

fn parse_interval(s: &str) -> Result<Duration, ScheduleError> {
    let s = s.trim();
    if s.len() < 2 {
        return Err(ScheduleError::BadInterval(s.into()));
    }
    let (n_str, unit) = s.split_at(s.len() - 1);
    let n: i64 = n_str
        .trim()
        .parse()
        .map_err(|_| ScheduleError::BadInterval(s.into()))?;
    match unit {
        "s" => Ok(Duration::seconds(n)),
        "m" => Ok(Duration::minutes(n)),
        "h" => Ok(Duration::hours(n)),
        "d" => Ok(Duration::days(n)),
        _ => Err(ScheduleError::BadInterval(s.into())),
    }
}

// =================================================================
// Daemon runtime
// =================================================================

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("config: {0}")]
    Config(String),
    #[error("schedule: {0}")]
    Schedule(#[from] ScheduleError),
    #[error("job `{name}` has neither `command` nor `argv`")]
    NoCommand { name: String },
}

pub struct Daemon {
    pub jobs: Vec<ResolvedJob>,
}

#[derive(Debug, Clone)]
pub struct ResolvedJob {
    pub name: String,
    pub schedule: Schedule,
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
}

impl Daemon {
    pub fn from_config(cfg: DaemonConfig) -> Result<Self, DaemonError> {
        let mut resolved = Vec::new();
        for j in cfg.jobs {
            if !j.enabled {
                continue;
            }
            let argv = match (j.argv, j.command) {
                (Some(a), _) if !a.is_empty() => a,
                (_, Some(c)) => c.split_whitespace().map(String::from).collect(),
                _ => {
                    return Err(DaemonError::NoCommand {
                        name: j.name.clone(),
                    });
                }
            };
            resolved.push(ResolvedJob {
                name: j.name,
                schedule: Schedule::parse(&j.schedule)?,
                argv,
                env: j.env,
                cwd: j.cwd,
            });
        }
        Ok(Daemon { jobs: resolved })
    }

    /// Print a summary of next fire times for every job, then exit.
    /// Useful for debugging a config without actually running anything.
    pub fn dry_run(&self) {
        let now = Local::now();
        println!("now: {}", now.format("%Y-%m-%d %H:%M:%S %Z"));
        if self.jobs.is_empty() {
            println!("(no enabled jobs)");
            return;
        }
        for j in &self.jobs {
            let next = j.schedule.next_after(now);
            let delta = next - now;
            println!(
                "  {:30}  next: {}  (in {})  cmd: {}",
                j.name,
                next.format("%Y-%m-%d %H:%M:%S"),
                fmt_delta(delta),
                j.argv.join(" "),
            );
        }
    }

    /// Run forever, firing each job at its scheduled time.
    /// Each job runs in its own tokio task so a slow one doesn't block others.
    pub async fn run(self) -> Result<(), DaemonError> {
        if self.jobs.is_empty() {
            tracing::warn!("no enabled jobs — daemon will idle until Ctrl-C");
        }
        let mut handles = Vec::new();
        for job in self.jobs {
            handles.push(tokio::spawn(run_job_loop(job)));
        }
        // Wait for Ctrl-C; if not running on a terminal that's OK — just await forever.
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Ctrl-C received, shutting down");
        for h in handles {
            h.abort();
        }
        Ok(())
    }
}

async fn run_job_loop(job: ResolvedJob) {
    let started = Local::now();
    tracing::info!(job = %job.name, "scheduled (next: {})",
        job.schedule.next_after(started).format("%Y-%m-%d %H:%M:%S"));

    loop {
        let now = Local::now();
        let next = job.schedule.next_after(now);
        let wait = (next - now)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(1));
        tokio::time::sleep(wait).await;

        let started = std::time::Instant::now();
        let result = run_once(&job).await;
        let elapsed_ms = started.elapsed().as_millis();

        match result {
            Ok(status) if status.success() => {
                tracing::info!(job = %job.name, ms = elapsed_ms, "✓ ok");
            }
            Ok(status) => {
                tracing::warn!(job = %job.name, ms = elapsed_ms, code = ?status.code(), "✗ non-zero exit");
            }
            Err(e) => {
                tracing::error!(job = %job.name, ms = elapsed_ms, error = %e, "✗ spawn failed");
            }
        }

        // For interval schedules, "next" was relative to (already-elapsed) now.
        // Don't busy-loop if job took longer than the interval.
    }
}

async fn run_once(job: &ResolvedJob) -> std::io::Result<std::process::ExitStatus> {
    let mut cmd = Command::new(&job.argv[0]);
    if job.argv.len() > 1 {
        cmd.args(&job.argv[1..]);
    }
    for (k, v) in &job.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &job.cwd {
        cmd.current_dir(cwd);
    }
    cmd.status().await
}

fn fmt_delta(d: Duration) -> String {
    let total = d.num_seconds();
    if total < 0 {
        return "due".into();
    }
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

// =================================================================
// Tests
// =================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_daily() {
        assert_eq!(
            Schedule::parse("daily 08:00").unwrap(),
            Schedule::Daily { hour: 8, minute: 0 }
        );
        assert_eq!(
            Schedule::parse("daily 23:59").unwrap(),
            Schedule::Daily {
                hour: 23,
                minute: 59
            }
        );
        assert!(Schedule::parse("daily 25:00").is_err());
        assert!(Schedule::parse("daily noon").is_err());
    }

    #[test]
    fn parse_weekly() {
        let s = Schedule::parse("weekly mon 09:30").unwrap();
        assert_eq!(
            s,
            Schedule::Weekly {
                weekday: Weekday::Mon,
                hour: 9,
                minute: 30
            }
        );
        assert!(Schedule::parse("weekly funday 09:30").is_err());
    }

    #[test]
    fn parse_interval() {
        assert_eq!(
            Schedule::parse("every 30s").unwrap(),
            Schedule::Interval(Duration::seconds(30))
        );
        assert_eq!(
            Schedule::parse("every 5m").unwrap(),
            Schedule::Interval(Duration::minutes(5))
        );
        assert_eq!(
            Schedule::parse("every 2h").unwrap(),
            Schedule::Interval(Duration::hours(2))
        );
        assert_eq!(
            Schedule::parse("every 1d").unwrap(),
            Schedule::Interval(Duration::days(1))
        );
        assert!(Schedule::parse("every 5min").is_err());
    }

    #[test]
    fn daily_next_after() {
        // 9am today, schedule is daily 8am → next is 8am tomorrow
        let now = Local.with_ymd_and_hms(2026, 5, 16, 9, 0, 0).unwrap();
        let next = Schedule::Daily { hour: 8, minute: 0 }.next_after(now);
        assert_eq!(next.date_naive(), now.date_naive() + Duration::days(1));
        assert_eq!(next.hour(), 8);
        // 7am today, schedule is daily 8am → next is 8am today
        let now = Local.with_ymd_and_hms(2026, 5, 16, 7, 0, 0).unwrap();
        let next = Schedule::Daily { hour: 8, minute: 0 }.next_after(now);
        assert_eq!(next.date_naive(), now.date_naive());
        assert_eq!(next.hour(), 8);
    }

    #[test]
    fn weekly_next_after() {
        // Saturday 2026-05-16. Schedule: weekly mon 09:30 → next is Mon 2026-05-18 09:30
        let sat = Local.with_ymd_and_hms(2026, 5, 16, 12, 0, 0).unwrap();
        assert_eq!(sat.weekday(), Weekday::Sat);
        let next = Schedule::Weekly {
            weekday: Weekday::Mon,
            hour: 9,
            minute: 30,
        }
        .next_after(sat);
        assert_eq!(next.weekday(), Weekday::Mon);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn config_roundtrip() {
        let toml_src = r#"
[[job]]
name = "brief"
schedule = "daily 08:00"
argv = ["assistant", "--brief"]

[[job]]
name = "disabled-one"
schedule = "every 5m"
command = "echo hi"
enabled = false
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.jobs.len(), 2);
        let daemon = Daemon::from_config(cfg).unwrap();
        // disabled one dropped
        assert_eq!(daemon.jobs.len(), 1);
        assert_eq!(daemon.jobs[0].name, "brief");
        assert_eq!(daemon.jobs[0].argv, vec!["assistant", "--brief"]);
    }

    // ====== audit #9: edge cases ======

    #[test]
    fn parse_zero_argv_falls_back_to_command_string() {
        // `command = "echo hi there"` → split by whitespace
        let toml_src = r#"
[[job]]
name = "echo"
schedule = "every 1m"
command = "echo hi there"
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        let d = Daemon::from_config(cfg).unwrap();
        assert_eq!(d.jobs.len(), 1);
        assert_eq!(d.jobs[0].argv, vec!["echo", "hi", "there"]);
    }

    #[test]
    fn parse_argv_wins_over_command_when_both_present() {
        let toml_src = r#"
[[job]]
name = "either-or"
schedule = "every 1m"
argv = ["cmd-a", "arg-1"]
command = "cmd-b arg-2"
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        let d = Daemon::from_config(cfg).unwrap();
        assert_eq!(d.jobs[0].argv, vec!["cmd-a", "arg-1"]);
    }

    #[test]
    fn parse_empty_argv_then_command_is_used() {
        let toml_src = r#"
[[job]]
name = "fallthrough"
schedule = "every 1m"
argv = []
command = "echo from-command"
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        let d = Daemon::from_config(cfg).unwrap();
        assert_eq!(d.jobs[0].argv, vec!["echo", "from-command"]);
    }

    #[test]
    fn parse_job_with_neither_argv_nor_command_errors() {
        let toml_src = r#"
[[job]]
name = "broken"
schedule = "every 1m"
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        let r = Daemon::from_config(cfg);
        match r {
            Err(DaemonError::NoCommand { name }) => assert_eq!(name, "broken"),
            Err(e) => panic!("expected NoCommand, got: {e}"),
            Ok(_) => panic!("expected NoCommand, got Ok"),
        }
    }

    #[test]
    fn parse_bad_schedule_propagates() {
        let toml_src = r#"
[[job]]
name = "x"
schedule = "every 5min"
command = "true"
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        let r = Daemon::from_config(cfg);
        assert!(matches!(r, Err(DaemonError::Schedule(_))));
    }

    #[test]
    fn weekly_wraps_to_next_week_when_today_is_after_target_time() {
        // Today is Mon 12:00, schedule is "weekly mon 09:30" — target already
        // passed today, so next fire is next Mon, NOT later today.
        let mon = Local.with_ymd_and_hms(2026, 5, 18, 12, 0, 0).unwrap();
        assert_eq!(mon.weekday(), Weekday::Mon);
        let next = Schedule::Weekly {
            weekday: Weekday::Mon,
            hour: 9,
            minute: 30,
        }
        .next_after(mon);
        assert_eq!(next.weekday(), Weekday::Mon);
        assert_eq!(next.date_naive(), mon.date_naive() + Duration::days(7));
    }

    #[test]
    fn daily_midnight_boundary() {
        // 23:30 today, schedule daily 00:05 → next is 00:05 tomorrow
        let late = Local.with_ymd_and_hms(2026, 5, 16, 23, 30, 0).unwrap();
        let next = Schedule::Daily { hour: 0, minute: 5 }.next_after(late);
        assert_eq!(next.date_naive(), late.date_naive() + Duration::days(1));
        assert_eq!(next.hour(), 0);
        assert_eq!(next.minute(), 5);
    }

    #[test]
    fn interval_next_is_always_now_plus_d() {
        // Interval schedules don't anchor to a wall-clock time — they just
        // measure from "now". This is a regression test against the temptation
        // to anchor them.
        let now = Local::now();
        let s = Schedule::Interval(Duration::seconds(30));
        let next = s.next_after(now);
        let delta = next - now;
        assert!(
            delta >= Duration::seconds(29) && delta <= Duration::seconds(31),
            "expected ~30s, got {delta:?}"
        );
    }

    #[test]
    fn fmt_delta_handles_hours_minutes_seconds() {
        assert_eq!(fmt_delta(Duration::seconds(5)), "5s");
        assert_eq!(fmt_delta(Duration::seconds(65)), "1m 5s");
        assert_eq!(fmt_delta(Duration::seconds(3725)), "1h 2m");
        assert_eq!(fmt_delta(Duration::seconds(-10)), "due");
    }

    #[test]
    fn env_preserved_in_resolved_job() {
        let toml_src = r#"
[[job]]
name = "with-env"
schedule = "every 1m"
command = "env"
env = { FOO = "bar", BAZ = "qux" }
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        let d = Daemon::from_config(cfg).unwrap();
        assert_eq!(d.jobs[0].env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(d.jobs[0].env.get("BAZ").map(String::as_str), Some("qux"));
    }

    #[test]
    fn cwd_preserved_in_resolved_job() {
        let toml_src = r#"
[[job]]
name = "with-cwd"
schedule = "every 1m"
command = "pwd"
cwd = "/tmp"
"#;
        let cfg: DaemonConfig = toml::from_str(toml_src).unwrap();
        let d = Daemon::from_config(cfg).unwrap();
        assert_eq!(d.jobs[0].cwd.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn weekday_aliases_all_accepted() {
        // Both short and long forms parse.
        for s in ["mon", "monday", "Mon", "MONDAY"] {
            assert!(matches!(
                Schedule::parse(&format!("weekly {s} 09:00")).unwrap(),
                Schedule::Weekly {
                    weekday: Weekday::Mon,
                    ..
                }
            ));
        }
        for s in ["sun", "sunday"] {
            assert!(matches!(
                Schedule::parse(&format!("weekly {s} 09:00")).unwrap(),
                Schedule::Weekly {
                    weekday: Weekday::Sun,
                    ..
                }
            ));
        }
    }

    /// Spawn an actual `true` subprocess via the daemon's run_once and confirm
    /// the env + cwd are propagated. Uses a Bash-built-in (well, /bin/true)
    /// so the test doesn't depend on any sysadmin binary beyond what every
    /// Unix box has.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_once_executes_subprocess_with_env_and_cwd() {
        let tmp = std::env::temp_dir().join(format!(
            "harness-daemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        // Write a marker via sh -c "echo $FOO > marker"; verify the env var
        // and cwd land where we expect.
        let job = ResolvedJob {
            name: "test".into(),
            schedule: Schedule::Interval(Duration::seconds(60)),
            argv: vec!["sh".into(), "-c".into(), "echo \"$FOO\" > marker".into()],
            env: std::collections::HashMap::from([("FOO".into(), "hello-edge".into())]),
            cwd: Some(tmp.clone()),
        };
        let status = run_once(&job).await.expect("spawn");
        assert!(status.success(), "subprocess exit: {status:?}");
        let marker = std::fs::read_to_string(tmp.join("marker")).unwrap();
        assert_eq!(marker.trim(), "hello-edge");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
