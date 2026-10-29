//! What wakes a host that is not in the middle of a run.
//!
//! A run is a loop the *model* drives; between runs nothing does. A background
//! job that finishes, or runs long, has no one to tell — the agent said "I'll
//! let you know when CI is done" and then the REPL sat in `read_line`. The fix
//! is on the host side: every source of "something happened" (keyboard, job
//! table, later a scheduler or webhook) feeds one queue of [`HostEvent`]s, and
//! the host's event loop decides what each does:
//!
//! - **idle** — start a run with the event as its input;
//! - **busy** — park it in a [`NoticeGuide`], which hands it to the model
//!   before its next call, without waiting for the run to end.
//!
//! The guide only ever *appends* a user turn to the end of the history. It
//! never edits what is already there, so a provider's prompt cache keeps
//! matching the prefix.

use async_trait::async_trait;
use harness_core::{
    Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, Turn, TurnRole, World,
};
use harness_tools::shell::JobEvent;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One thing that happened to the host. Not `harness_core::Event`, which is
/// the hook enum fired from *inside* a run; these arrive from outside it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HostEvent {
    /// A line the person typed.
    Input(String),
    /// A background job ended.
    JobExited {
        id: u64,
        program: String,
        exit_status: i32,
        running_for: Duration,
        tail: String,
    },
    /// A background job is still running past the time it was expected to take.
    JobOverdue {
        id: u64,
        program: String,
        running_for: Duration,
        tail: String,
    },
}

impl From<JobEvent> for HostEvent {
    fn from(ev: JobEvent) -> Self {
        match ev {
            JobEvent::Exited {
                id,
                program,
                exit_status,
                running_for,
                tail,
            } => HostEvent::JobExited {
                id,
                program,
                exit_status,
                running_for,
                tail,
            },
            JobEvent::Overdue {
                id,
                program,
                running_for,
                tail,
            } => HostEvent::JobOverdue {
                id,
                program,
                running_for,
                tail,
            },
            // `JobEvent` is non_exhaustive; a kind this host does not know is
            // reported as what it is rather than dropped.
            other => HostEvent::Input(format!("[unrecognised job event: {other:?}]")),
        }
    }
}

impl HostEvent {
    /// One line for the person at the terminal; `None` for typed input, which
    /// they already saw. E.g. `job 3 (gh) exited 0 after 21m`.
    pub fn headline(&self) -> Option<String> {
        match self {
            HostEvent::JobExited {
                id,
                program,
                exit_status,
                running_for,
                ..
            } => Some(format!(
                "job {id} ({program}) exited {exit_status} after {}",
                fmt_elapsed(*running_for)
            )),
            HostEvent::JobOverdue {
                id,
                program,
                running_for,
                ..
            } => Some(format!(
                "job {id} ({program}) still running after {}",
                fmt_elapsed(*running_for)
            )),
            _ => None,
        }
    }

    /// The text the model reads for this event: what happened, the tail of the
    /// job's output, and what is expected of it. `None` for typed input, which
    /// is already the model's input as it stands.
    pub fn notice(&self) -> Option<String> {
        let (head, tail, ask) = match self {
            HostEvent::JobExited {
                tail, exit_status, ..
            } => (
                self.headline()?,
                tail,
                if *exit_status == 0 {
                    "Tell the user the result.".to_string()
                } else {
                    "Tell the user it failed and why, from the output.".to_string()
                },
            ),
            HostEvent::JobOverdue { id, tail, .. } => (
                self.headline()?,
                tail,
                // Overdue is a question, not a verdict: the job may be fine.
                format!(
                    "Check it with shell_job_status (id {id}); tell the user whether it is \
                     progressing, and kill it with shell_job_kill if it is stuck."
                ),
            ),
            _ => return None,
        };
        let mut s = format!("[background job notice] {head}.");
        if !tail.trim().is_empty() {
            s.push_str(&format!("\nLast output:\n{}", tail.trim_end()));
        }
        s.push('\n');
        s.push_str(&ask);
        Some(s)
    }
}

/// `45s`, `21m`, `1h05m` — coarse on purpose, it is read at a glance.
pub fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// Notices waiting for the model, delivered before its next call.
///
/// Share one `Arc<NoticeGuide>`: register a clone on the loop with
/// `with_guide`, keep the other to [`push`](Self::push) from the host's event
/// loop. Delivery is append-only — one user turn at the end of the history.
pub struct NoticeGuide {
    id: GuideId,
    scope: GuideScope,
    state: Mutex<NoticeState>,
}

#[derive(Default)]
struct NoticeState {
    pending: Vec<String>,
    /// What has been handed to the model since the host last asked. A run's
    /// context is thrown away when it ends, so the host needs this to keep the
    /// notices in the conversation it carries into the next run.
    delivered: Vec<String>,
}

impl NoticeGuide {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            id: "host-notices".into(),
            scope: GuideScope::Always,
            state: Mutex::new(NoticeState::default()),
        })
    }

    /// Queue a notice for the model's next call.
    pub fn push(&self, text: impl Into<String>) {
        self.state.lock().unwrap().pending.push(text.into());
    }

    /// Anything not yet shown to the model? A run that ends with notices
    /// still pending never gave the model the chance to read them.
    pub fn has_pending(&self) -> bool {
        !self.state.lock().unwrap().pending.is_empty()
    }

    /// Take what is still waiting (e.g. to use as the next run's input).
    pub fn take_pending(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().pending)
    }

    /// Take what the guide has delivered since the last call.
    pub fn take_delivered(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().delivered)
    }
}

#[async_trait]
impl Guide for NoticeGuide {
    fn id(&self) -> &GuideId {
        &self.id
    }
    fn kind(&self) -> Execution {
        Execution::Computational
    }
    fn scope(&self) -> &GuideScope {
        &self.scope
    }
    async fn apply(&self, _ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        Ok(())
    }
    async fn apply_before_iter(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        let mut st = self.state.lock().unwrap();
        if st.pending.is_empty() {
            return Ok(());
        }
        let notices = std::mem::take(&mut st.pending);
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(notices.join("\n\n"))],
        });
        st.delivered.extend(notices);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Task;

    fn ctx() -> Context {
        Context::new(Task {
            description: "t".into(),
            source: None,
            deadline: None,
        })
    }

    fn exited(code: i32) -> HostEvent {
        HostEvent::JobExited {
            id: 3,
            program: "gh".into(),
            exit_status: code,
            running_for: Duration::from_secs(21 * 60 + 5),
            tail: "all checks passed\n".into(),
        }
    }

    #[test]
    fn headline_is_one_short_line() {
        assert_eq!(
            exited(0).headline().unwrap(),
            "job 3 (gh) exited 0 after 21m"
        );
        assert_eq!(HostEvent::Input("hi".into()).headline(), None);
        assert_eq!(fmt_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_elapsed(Duration::from_secs(3900)), "1h05m");
    }

    #[test]
    fn notice_carries_the_tail_and_what_to_do() {
        let n = exited(1).notice().unwrap();
        assert!(n.contains("job 3 (gh) exited 1"));
        assert!(n.contains("all checks passed"));
        assert!(n.contains("failed"));
        let overdue = HostEvent::JobOverdue {
            id: 7,
            program: "make".into(),
            running_for: Duration::from_secs(600),
            tail: String::new(),
        };
        let n = overdue.notice().unwrap();
        assert!(n.contains("shell_job_status (id 7)"));
        assert!(!n.contains("Last output"));
        assert!(HostEvent::Input("x".into()).notice().is_none());
    }

    #[tokio::test]
    async fn guide_appends_one_user_turn_and_only_when_pending() {
        let g = NoticeGuide::new();
        let w = harness_context::default_world(".");
        let mut c = ctx();
        c.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text("earlier".into())],
        });

        g.apply_before_iter(&mut c, &w).await.unwrap();
        assert_eq!(c.history.len(), 1, "nothing pending, nothing appended");

        g.push("one");
        g.push("two");
        assert!(g.has_pending());
        g.apply_before_iter(&mut c, &w).await.unwrap();
        assert_eq!(c.history.len(), 2);
        assert_eq!(c.history[0].blocks.len(), 1, "earlier history untouched");
        assert!(matches!(c.history[1].role, TurnRole::User));
        let Block::Text(t) = &c.history[1].blocks[0] else {
            panic!("expected text")
        };
        assert!(t.contains("one") && t.contains("two"));
        assert!(!g.has_pending());
        assert_eq!(g.take_delivered(), vec!["one", "two"]);
        assert!(g.take_delivered().is_empty());
    }
}
