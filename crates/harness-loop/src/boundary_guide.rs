//! Opt-in `Guide` for the requests an agent *cannot* fulfil.
//!
//! An agent is given a tool belt and a goal, and the loop is built to keep
//! going until one of them runs out. Nothing in that arrangement tells it what
//! to do when the user asks for something no tool can reach — order dinner,
//! phone someone, move money — and the observed behaviour is worse than a
//! refusal.
//!
//! Measured on a 50-task assistant benchmark, two different agents on the same
//! model, asked to book a hotel / place an order / call a client / transfer
//! funds: both said they could not, and then *did something else anyway*. One
//! opened with "I've noted down your plan for a two-night stay in Paris!" and
//! left the limitation as a subordinate clause. Another answered "I have logged
//! this $500 transfer in your records" — which a person reads as money having
//! moved. Between them they spent up to six tool calls per refusal.
//!
//! The cause is ordinary and worth naming: an app whose prompt says "always
//! remember what matters" gets that instruction obeyed hardest exactly when
//! there is nothing else to obey. Filing a note is the only available action,
//! so the agent takes it, and the refusal ends up buried under activity.
//!
//! ```ignore
//! AgentLoop::new(model)
//!     .with_guide(std::sync::Arc::new(harness_loop::BoundaryGuide))
//! ```

use async_trait::async_trait;
use harness_core::{Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, World};
use std::sync::OnceLock;

/// Tells the agent to lead with the limitation and stop there.
///
/// Deliberately phrased as a test the model applies — "would this act on the
/// world?" — rather than a list of forbidden verbs. A list is a thing to be
/// outside of; the question generalises to whatever the app's tool belt does
/// not cover.
pub struct BoundaryGuide;

static ID: OnceLock<GuideId> = OnceLock::new();
static SCOPE: OnceLock<GuideScope> = OnceLock::new();

/// The guidance, exposed so an app can fold it into its own system prompt
/// instead of attaching the guide.
pub const BOUNDARY_GUIDANCE: &str = "\
When a request needs an ability you do not have, say so in your FIRST sentence, \
then offer what you can actually do. The test is whether fulfilling it would act \
on the world — placing an order, paying, moving money, contacting someone on the \
user's behalf — not whether it sounds difficult.\n\
Never put \"I've noted that down\" or \"I've saved it to your records\" ahead of \
the limitation: the user reads that as the thing having been done. \"I have \
logged this transfer in your records\" is the shape to avoid.\n\
And do not substitute activity for help. Saving a memory, creating an entity or \
writing a record because you could not do the real thing is noise the user did \
not ask for. An honest \"I can't do that, but here is what I can do\" is the \
complete answer.";

#[async_trait]
impl Guide for BoundaryGuide {
    fn id(&self) -> &GuideId {
        ID.get_or_init(|| "capability-boundary".into())
    }
    fn kind(&self) -> Execution {
        Execution::Inferential
    }
    fn scope(&self) -> &GuideScope {
        SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        ctx.guides.push(Block::Text(BOUNDARY_GUIDANCE.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Task;

    #[tokio::test]
    async fn it_says_refusal_comes_first_and_alone() {
        let mut ctx = Context::new(Task {
            description: "book me a hotel".into(),
            source: None,
            deadline: None,
        });
        let world = harness_context::default_world(".");
        BoundaryGuide.apply(&mut ctx, &world).await.unwrap();

        let text = ctx
            .guides
            .iter()
            .filter_map(|b| match b {
                Block::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<String>();

        assert!(text.contains("FIRST sentence"), "order matters");
        // The two halves that the benchmark showed agents getting wrong.
        assert!(
            text.contains("noted that down"),
            "no filing before the refusal"
        );
        assert!(
            text.contains("substitute activity"),
            "no busywork instead of help"
        );
        // A list of banned verbs would be a thing to be outside of; the test
        // has to generalise past whatever this app's tools happen to cover.
        assert!(
            text.contains("act on the world"),
            "phrased as a test, not a list"
        );
    }
}
