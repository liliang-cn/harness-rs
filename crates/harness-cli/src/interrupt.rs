//! Ctrl-C as a cancel, not a kill.
//!
//! A shell tool's child runs in its own process group so a cancelled run can
//! kill everything it started — which also means a terminal Ctrl-C no longer
//! reaches that child by group propagation. If the harness simply died on
//! SIGINT, the child would be orphaned — exactly the failure that run
//! cancellation exists to prevent, only now triggered by Ctrl-C instead of by
//! dropping the run. So the CLI takes SIGINT itself and turns it into a cancel
//! of whatever run is in flight; the loop unwinds, drops the tool, and the
//! group dies with it.

use harness_loop::CancellationToken;
use std::sync::{Arc, Mutex};

/// The run currently entitled to be cancelled by Ctrl-C, if any.
///
/// `harness run` arms it once. The REPL arms it per turn and disarms between
/// turns, because a token is one-way: cancelling a loop-lifetime token on turn
/// three would make every later turn return `Cancelled` on entry.
#[derive(Clone, Default)]
pub struct Current(Arc<Mutex<Option<CancellationToken>>>);

impl Current {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand Ctrl-C a fresh token for the run about to start, and return it
    /// for `AgentLoop::cancel` / `with_cancellation`.
    pub fn arm(&self) -> CancellationToken {
        let token = CancellationToken::new();
        *self.0.lock().unwrap() = Some(token.clone());
        token
    }

    /// The run is over. Until the next `arm`, Ctrl-C means "quit".
    pub fn disarm(&self) {
        self.0.lock().unwrap().take();
    }

    /// One Ctrl-C. Cancels the armed run if there is one and says whether
    /// there was; the caller decides what an idle Ctrl-C means.
    pub fn interrupt(&self) -> bool {
        let taken = self.0.lock().unwrap().take();
        match taken {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

/// Watch for Ctrl-C for the life of the process. Each one cancels the armed
/// run and calls `on_cancel`; one that finds nothing armed calls `on_idle`.
/// The CLI passes an exit with status 130 — 128 + SIGINT, the shell's own code
/// for "interrupted" — so a Ctrl-C at an idle prompt quits, and a second
/// Ctrl-C during a cancel that is slow to unwind forces the process down.
///
/// That forced exit skips destructors, so a child process still unwinding at
/// that moment is orphaned in its own group — the deliberate price of an
/// escape hatch, which is why `on_cancel` tells the user the first press was
/// heard.
///
/// Installing the handler replaces SIGINT's default disposition for the whole
/// process, which is why `on_idle` has to exist: without it, an idle Ctrl-C
/// would be swallowed.
///
/// The watcher only makes progress because the runtime is multi-threaded: the
/// REPL prompt blocks a worker in `read_line`, so on a `current_thread`
/// runtime a Ctrl-C at the prompt would be dead.
pub fn watch(
    current: Current,
    on_cancel: impl Fn() + Send + 'static,
    on_idle: impl Fn() + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = tokio::signal::ctrl_c().await {
                // No signal support on this platform/terminal: leave the
                // default disposition in place rather than pretend.
                tracing::warn!(error = %e, "ctrl-c handler unavailable; SIGINT keeps its default disposition");
                return;
            }
            if current.interrupt() {
                on_cancel();
            } else {
                on_idle();
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Current;

    #[test]
    fn a_ctrl_c_cancels_the_armed_run() {
        let current = Current::new();
        let token = current.arm();
        assert!(!token.is_cancelled());

        assert!(current.interrupt(), "there was a run to cancel");
        assert!(token.is_cancelled(), "the armed token must be cancelled");
        assert!(!current.interrupt(), "the same run is not cancelled twice");
    }

    #[test]
    fn a_ctrl_c_with_nothing_armed_is_reported_as_idle() {
        let current = Current::new();
        assert!(!current.interrupt());
    }

    // The REPL disarms between turns: a Ctrl-C at the prompt must not reach
    // into a run that already finished — and must not poison the next one.
    #[test]
    fn a_disarmed_token_is_left_alone() {
        let current = Current::new();
        let token = current.arm();
        current.disarm();

        assert!(!current.interrupt(), "nothing armed, so idle");
        assert!(
            !token.is_cancelled(),
            "the finished turn's token is untouched"
        );
    }

    // The watcher holds a clone; the run holds the original. They must see
    // the same slot or Ctrl-C cancels nothing.
    #[test]
    fn a_clone_shares_the_slot() {
        let current = Current::new();
        let watcher = current.clone();
        let token = current.arm();

        assert!(watcher.interrupt(), "the clone sees the armed run");
        assert!(token.is_cancelled());
        assert!(!current.interrupt(), "and the original sees it was taken");
    }
}
