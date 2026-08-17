//! What "done" means, checked before the loop says so.
//!
//! An agent loop ends when the model stops asking for tools. That is the right
//! rule and it answers the wrong question: it tells you the model believes it
//! is finished, not that the work happened. A run that narrates a plan and
//! stops looks exactly like a run that did the job.
//!
//! `harness-loop-engine` already draws this distinction — its maker/checker
//! split is the same discipline — but only if you adopt that whole runtime.
//! Most code uses a bare [`crate::AgentLoop`], and there the question wasn't
//! asked at all. An [`Acceptance`] is that question, on the loop everyone
//! actually uses: consulted before `Outcome::Done`, and when it says no, the
//! reason goes back to the model and the loop carries on.
//!
//! Keep them cheap and deterministic where you can — a file either exists or
//! it doesn't, and that costs no tokens. A second agent as judge is possible
//! (the host supplies it) but it's the expensive end, not the default.

use async_trait::async_trait;
use harness_core::{Context, World};

/// The answer to "is this actually done?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    /// Why. When it did not pass this goes back to the model verbatim, so
    /// write it as an instruction it can act on, not as a complaint.
    pub reason: String,
}

impl Verdict {
    pub fn passed() -> Self {
        Self {
            passed: true,
            reason: String::new(),
        }
    }
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            passed: false,
            reason: reason.into(),
        }
    }
}

/// A condition the run must satisfy before the loop reports success.
#[async_trait]
pub trait Acceptance: Send + Sync + 'static {
    /// Short name, for logs.
    fn name(&self) -> &str;

    /// Consulted when the model has stopped asking for tools. `ctx` carries the
    /// task and the whole transcript; `world` is the filesystem and friends.
    async fn check(&self, ctx: &Context, world: &World) -> Verdict;

    /// The files whose contents *define* this check.
    ///
    /// Declaring them seals them: the loop digests each one before the model's
    /// first turn and again before accepting a pass, and a difference fails the
    /// run regardless of the verdict. This exists because a gate the agent can
    /// edit is not a gate — the standard failure is a model that cannot make a
    /// test pass widening the test until it does.
    ///
    /// Relative paths resolve against `world.repo.root`. Empty by default: some
    /// runs are *supposed* to rewrite their tests, and a check cannot know
    /// which kind of run it is in. Seal what must not move. See [`crate::seal`]
    /// for exactly what the seal does and does not enforce.
    fn seals(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

/// The cheapest check there is: the turn produced *something*.
///
/// This is the shape that prompted the whole idea — no tool calls, no text,
/// just reasoning the model narrated to itself and then stopped. Left
/// unchecked, that monologue gets handed back as the answer.
pub struct NonEmptyAnswer;

#[async_trait]
impl Acceptance for NonEmptyAnswer {
    fn name(&self) -> &str {
        "non-empty-answer"
    }

    async fn check(&self, ctx: &Context, _world: &World) -> Verdict {
        let said_something = ctx
            .history
            .last()
            .filter(|t| t.role == harness_core::TurnRole::Assistant)
            .is_some_and(|t| {
                t.blocks.iter().any(|b| match b {
                    harness_core::Block::Text(s) => !s.trim().is_empty(),
                    _ => false,
                })
            });
        if said_something {
            Verdict::passed()
        } else {
            Verdict::failed(
                "You ended that turn without answering and without calling a tool — you only \
                 thought about it. Nothing you described has actually been done yet. Carry on \
                 now: call the tools you need, and when the work is really finished, say so.",
            )
        }
    }
}

/// Every named path must exist under the workspace root, and be non-empty.
///
/// "Convert this PDF to Word" has an objective answer that costs no tokens to
/// check: is there a .docx, and does it have bytes in it?
pub struct FilesExist {
    paths: Vec<String>,
}

impl FilesExist {
    pub fn new<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            paths: paths.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl Acceptance for FilesExist {
    fn name(&self) -> &str {
        "files-exist"
    }

    async fn check(&self, _ctx: &Context, world: &World) -> Verdict {
        let root = &world.repo.root;
        let missing: Vec<&str> = self
            .paths
            .iter()
            .filter(|p| {
                let full = root.join(p);
                // Present but empty is the same failure as absent: a 0-byte
                // file is what a half-finished write leaves behind.
                !std::fs::metadata(&full)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
            })
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            Verdict::passed()
        } else {
            Verdict::failed(format!(
                "These files were expected but are missing or empty: {}. \
                 The work is not done — create them properly, then say so.",
                missing.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Block, Turn, TurnRole};

    fn ctx_with(blocks: Vec<Block>) -> Context {
        let mut c = Context::new(harness_core::Task {
            description: "do it".into(),
            source: None,
            deadline: None,
        });
        c.history.push(Turn {
            role: TurnRole::Assistant,
            blocks,
        });
        c
    }

    #[tokio::test]
    async fn thinking_out_loud_is_not_an_answer() {
        let world = harness_context::default_world(".");

        let only_reasoning = ctx_with(vec![Block::Reasoning("I'll write the docx next".into())]);
        let v = NonEmptyAnswer.check(&only_reasoning, &world).await;
        assert!(!v.passed);
        assert!(v.reason.contains("Carry on"), "the reason is actionable");

        // Whitespace is not text either.
        let blank = ctx_with(vec![Block::Text("   \n".into())]);
        assert!(!NonEmptyAnswer.check(&blank, &world).await.passed);

        let answered = ctx_with(vec![Block::Text("Done — resume.docx is written.".into())]);
        assert!(NonEmptyAnswer.check(&answered, &world).await.passed);
    }

    #[tokio::test]
    async fn a_promised_file_has_to_be_there_and_have_bytes_in_it() {
        let dir = std::env::temp_dir().join(format!("acceptance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let world = harness_context::default_world(&dir);
        let ctx = ctx_with(vec![Block::Text("all done!".into())]);

        let check = FilesExist::new(["out.docx"]);
        let v = check.check(&ctx, &world).await;
        assert!(!v.passed, "absent");
        assert!(v.reason.contains("out.docx"), "names what's missing: {v:?}");

        // Touched but empty is still not done.
        std::fs::write(dir.join("out.docx"), b"").unwrap();
        assert!(!check.check(&ctx, &world).await.passed, "empty");

        std::fs::write(dir.join("out.docx"), b"PK\x03\x04").unwrap();
        assert!(check.check(&ctx, &world).await.passed);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
