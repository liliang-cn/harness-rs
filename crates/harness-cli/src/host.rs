//! The decisions behind `harness code`'s event loop, kept free of I/O so they
//! can be tested without a terminal, a model or a process.
//!
//! The loop in `run_code` multiplexes the keyboard and the job table. What it
//! does with each arrival depends on only two facts — is a turn running, and
//! how many turns in a row has the host started on its own — and that is all
//! [`Dispatcher`] tracks.

use serde_json::Value;

/// How many turns the host may start by itself (a job event, with no one
/// typing) before it stops and waits for the person. Without a ceiling, an
/// agent that answers each notice by spawning another job would keep itself
/// busy forever, spending tokens nobody asked for.
pub const MAX_AUTO_TURNS: u32 = 3;

/// What to do with a job notice.
#[derive(Debug, PartialEq, Eq)]
pub enum JobAction {
    /// Idle and under the cap: run a turn with the notice as its input.
    StartTurn,
    /// Park it for the model's next call — a turn is running, or the cap is
    /// reached and the notice waits for the person's next message.
    Queue,
}

#[derive(Debug, Default)]
pub struct Dispatcher {
    busy: bool,
    auto_turns: u32,
}

impl Dispatcher {
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The person typed something: the next turn is theirs, and the count of
    /// unattended turns starts over.
    pub fn user_input(&mut self) {
        self.auto_turns = 0;
    }

    /// A turn started on the person's input.
    pub fn start_user_turn(&mut self) {
        self.busy = true;
    }

    /// A turn started with no one typing; false (and nothing changes) at the cap.
    fn try_start_auto_turn(&mut self) -> bool {
        if self.busy || self.auto_turns >= MAX_AUTO_TURNS {
            return false;
        }
        self.busy = true;
        self.auto_turns += 1;
        true
    }

    pub fn on_job_event(&mut self) -> JobAction {
        if self.try_start_auto_turn() {
            JobAction::StartTurn
        } else {
            JobAction::Queue
        }
    }

    /// The turn ended. `notices_pending`: the model never got to read some
    /// notice that arrived late in it. True means start a follow-up turn.
    pub fn finish_turn(&mut self, notices_pending: bool) -> bool {
        self.busy = false;
        notices_pending && self.try_start_auto_turn()
    }

    #[cfg(test)]
    fn at_cap(&self) -> bool {
        self.auto_turns >= MAX_AUTO_TURNS
    }
}

/// The `/jobs` listing: id, program, state, running time, and the deadline the
/// host will next be woken at.
pub fn fmt_jobs(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "no background jobs".to_string();
    }
    let mut out = String::new();
    for r in rows {
        let id = r["id"].as_u64().unwrap_or(0);
        let mut cmd = r["program"].as_str().unwrap_or("?").to_string();
        for a in r["args"].as_array().into_iter().flatten() {
            cmd.push(' ');
            cmd.push_str(a.as_str().unwrap_or(""));
        }
        if cmd.chars().count() > 40 {
            cmd = format!("{}…", cmd.chars().take(40).collect::<String>());
        }
        let running_s = r["running_for_ms"].as_u64().unwrap_or(0) / 1000;
        let state = match r["exit_status"].as_i64() {
            Some(code) => format!("exited {code}"),
            None => {
                // Each Overdue re-arms for another interval.
                let due = r["expect_secs"].as_u64().unwrap_or(0)
                    * (r["overdue_count"].as_u64().unwrap_or(0) + 1);
                if running_s >= due {
                    "running, overdue".to_string()
                } else {
                    format!(
                        "running, due in {}",
                        harness_loop::events::fmt_elapsed(std::time::Duration::from_secs(
                            due - running_s
                        ))
                    )
                }
            }
        };
        out.push_str(&format!(
            "  {id:>3}  {cmd:<42} {:<22} {}\n",
            state,
            harness_loop::events::fmt_elapsed(std::time::Duration::from_secs(running_s))
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idle_job_event_starts_a_turn() {
        let mut d = Dispatcher::default();
        assert_eq!(d.on_job_event(), JobAction::StartTurn);
        assert!(d.is_busy());
    }

    #[test]
    fn busy_job_event_is_queued_and_a_follow_up_runs_after() {
        let mut d = Dispatcher::default();
        d.start_user_turn();
        assert_eq!(d.on_job_event(), JobAction::Queue);
        assert!(d.finish_turn(true), "pending notice must get a turn");
        assert!(d.is_busy());
        assert!(!d.finish_turn(false), "nothing pending: go idle");
        assert!(!d.is_busy());
    }

    #[test]
    fn no_follow_up_without_pending_notices() {
        let mut d = Dispatcher::default();
        d.start_user_turn();
        assert!(!d.finish_turn(false));
    }

    #[test]
    fn auto_turns_stop_at_the_cap_and_reset_on_input() {
        let mut d = Dispatcher::default();
        for _ in 0..MAX_AUTO_TURNS {
            assert_eq!(d.on_job_event(), JobAction::StartTurn);
            assert!(!d.finish_turn(false));
        }
        assert!(d.at_cap());
        assert_eq!(d.on_job_event(), JobAction::Queue, "cap reached");
        // Follow-ups count against the same cap.
        d.start_user_turn();
        assert!(!d.finish_turn(true));
        d.user_input();
        assert!(!d.at_cap());
        assert_eq!(d.on_job_event(), JobAction::StartTurn);
    }

    #[test]
    fn jobs_listing_shows_state_and_deadline() {
        let rows = vec![
            json!({"id": 3, "program": "gh", "args": ["run", "watch"], "exit_status": null,
                   "running_for_ms": 120_000, "expect_secs": 600, "overdue_count": 0}),
            json!({"id": 4, "program": "sleep", "args": ["9"], "exit_status": 0,
                   "running_for_ms": 9_000, "expect_secs": 10, "overdue_count": 0}),
            json!({"id": 5, "program": "make", "args": [], "exit_status": null,
                   "running_for_ms": 700_000, "expect_secs": 600, "overdue_count": 1}),
        ];
        let s = fmt_jobs(&rows);
        assert!(s.contains("gh run watch"), "{s}");
        assert!(s.contains("running, due in 8m"), "{s}");
        assert!(s.contains("exited 0"), "{s}");
        assert!(s.contains("make") && s.lines().count() == 3, "{s}");
        assert_eq!(fmt_jobs(&[]), "no background jobs");
    }
}
