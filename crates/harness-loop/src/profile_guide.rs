//! Opt-in `Guide` that renders [`harness_core::UserProfile`] into the agent's system prompt.
//!
//! Add to your loop when you want every model call to know who it's working for:
//!
//! ```ignore
//! AgentLoop::new(model)
//!     .with_guide(std::sync::Arc::new(harness_loop::ProfileGuide))
//!     .run(task, &mut world).await?;
//! ```
//!
//! The framework deliberately does NOT auto-attach this — `World.profile` is
//! a slot the app fills however it wants (env, CLI, app-side config), and
//! injection into the prompt is also the app's call.

use async_trait::async_trait;
use harness_core::{Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, World};
use std::sync::OnceLock;

/// Renders `World.profile` as a one-line `User profile: …` block in the
/// agent's `guides` section. No-op when the profile is empty.
pub struct ProfileGuide;

static PROFILE_GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static PROFILE_GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

#[async_trait]
impl Guide for ProfileGuide {
    fn id(&self) -> &GuideId {
        PROFILE_GUIDE_ID.get_or_init(|| "user-profile".into())
    }
    fn kind(&self) -> Execution {
        Execution::Inferential
    }
    fn scope(&self) -> &GuideScope {
        PROFILE_GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, w: &World) -> Result<(), GuideError> {
        let p = &w.profile;
        if p.name.is_none() && p.tz.is_none() && p.locale.is_none() && p.extra.is_empty() {
            return Ok(()); // empty profile — don't pollute the prompt
        }
        ctx.guides
            .push(Block::Text(format!("User profile: {}", p.summary_line())));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use harness_core::UserProfile;

    #[test]
    fn empty_profile_is_empty() {
        let p = UserProfile::default();
        assert!(p.name.is_none() && p.tz.is_none() && p.locale.is_none() && p.extra.is_empty());
    }

    #[test]
    fn populated_profile_renders_line() {
        let p = UserProfile {
            name: Some("李亮".into()),
            tz: Some("Asia/Shanghai".into()),
            locale: Some("zh-CN".into()),
            ..Default::default()
        };
        let s = p.summary_line();
        assert!(s.contains("李亮"));
        assert!(s.contains("Asia/Shanghai"));
        assert!(s.contains("zh-CN"));
    }
}
