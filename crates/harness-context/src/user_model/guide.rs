//! `UserModelGuide` — inject the portrait into the prompt, compactly, for one
//! named user.
//!
//! Modelled on `harness-experience`'s `ExperienceGuide`: the block is prefixed
//! with a unique marker, and every injection first strips the previous one, so
//! re-running across iterations replaces rather than appends. That is the only
//! thing standing between a long session and a system prompt that grows a copy
//! of the portrait per turn.
//!
//! One deliberate difference from `ExperienceGuide`: that guide re-queries its
//! store on every iteration because *what to recall depends on what was just
//! said*. A portrait does not — it is the same paragraph regardless of the
//! current message — so re-reading it from the backend each turn would be pure
//! I/O for an identical result. This guide loads once per session in
//! [`Guide::apply`], caches the rendered block, and
//! [`Guide::apply_before_iter`] only strips and re-injects the cached string.
//! Call [`UserModelGuide::invalidate`] after an update round to pick up a
//! freshly deepened portrait mid-session.

use super::{PortraitPolicy, UserId, UserModelStore};
use async_trait::async_trait;
use harness_core::{Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, World};
use std::sync::{Arc, Mutex, OnceLock};

const MARKER: &str = "[user-portrait]\n";

/// Renders one user's portrait into the prompt, under a hard char budget.
///
/// The guide is constructed **per user**: the id is a constructor argument, not
/// something read out of ambient state at render time. In a multi-tenant server
/// each session builds its own guide, so there is no code path where a portrait
/// could be rendered for a user other than the one named here.
pub struct UserModelGuide {
    store: Arc<UserModelStore>,
    user: UserId,
    policy: PortraitPolicy,
    id: GuideId,
    /// Rendered block (marker included), or `None` for "nothing to inject".
    /// The outer `Option` distinguishes "not loaded yet" from "loaded, empty".
    cached: Mutex<Option<Option<String>>>,
    /// Injected clock, so tests (and replayed sessions) can age beliefs
    /// deterministically instead of at whatever the wall clock says.
    now_ms: Option<i64>,
}

static GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

impl UserModelGuide {
    pub fn new(store: Arc<UserModelStore>, user: UserId) -> Self {
        // The id carries the user key so two guides for two users in the same
        // process are distinguishable in logs and in `ctx` diagnostics.
        let id = format!("user-portrait:{}", user.key());
        Self {
            store,
            user,
            policy: PortraitPolicy::default(),
            id,
            cached: Mutex::new(None),
            now_ms: None,
        }
    }

    /// Override the render budget / aging policy.
    pub fn with_policy(mut self, policy: PortraitPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Pin the clock used for confidence decay. Default: system time.
    pub fn with_now_ms(mut self, now_ms: i64) -> Self {
        self.now_ms = Some(now_ms);
        self
    }

    pub fn user(&self) -> &UserId {
        &self.user
    }

    /// Forget the cached render so the next injection re-reads the store. Call
    /// after an update round lands a new revision mid-session.
    pub fn invalidate(&self) {
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn now(&self) -> i64 {
        self.now_ms.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        })
    }

    /// The block that would be injected, loading and rendering if needed.
    /// Public so a server can log or preview exactly what a user's prompt will
    /// contain — a portrait you cannot inspect is a portrait you cannot debug.
    pub async fn block(&self) -> Option<String> {
        if let Some(hit) = self
            .cached
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return hit;
        }
        let model = match self.store.load(&self.user).await {
            Ok(m) => m,
            Err(e) => {
                // A portrait is an enhancement, never a precondition: a store
                // outage must degrade the prompt, not fail the session.
                tracing::warn!(user = %self.user, error = %e, "user portrait load failed");
                return None;
            }
        };
        debug_assert_eq!(model.user_id, self.user);
        let budget = self
            .policy
            .render_budget_chars
            .saturating_sub(MARKER.chars().count());
        let rendered = model
            .render_within(self.now(), &self.policy, budget)
            .map(|body| format!("{MARKER}{body}"));
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(rendered.clone());
        rendered
    }

    fn remove_previous(&self, ctx: &mut Context) {
        ctx.guides
            .retain(|b| !matches!(b, Block::Text(t) if t.starts_with(MARKER)));
    }

    async fn inject(&self, ctx: &mut Context) {
        self.remove_previous(ctx);
        if let Some(block) = self.block().await {
            ctx.guides.push(Block::Text(block));
        }
    }
}

#[async_trait]
impl Guide for UserModelGuide {
    fn id(&self) -> &GuideId {
        &self.id
    }
    fn kind(&self) -> Execution {
        Execution::Computational
    }
    fn scope(&self) -> &GuideScope {
        GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        self.inject(ctx).await;
        Ok(())
    }
    async fn apply_before_iter(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        self.inject(ctx).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        CommField, ConstraintKind, GoalStatus, IdentityField, Observation, UserModel,
        UserModelDelta,
    };
    use super::*;
    use async_trait::async_trait;
    use harness_core::{Memory, MemoryEntry, MemoryError, Task};
    use std::sync::Mutex as StdMutex;

    const DAY: i64 = 86_400_000;

    #[derive(Default)]
    struct VecMemory(StdMutex<Vec<MemoryEntry>>);
    #[async_trait]
    impl Memory for VecMemory {
        async fn recall(&self, _q: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
            Ok(self.0.lock().unwrap().iter().take(k).cloned().collect())
        }
        async fn write(&self, e: MemoryEntry) -> Result<(), MemoryError> {
            self.0.lock().unwrap().push(e);
            Ok(())
        }
    }

    fn ctx() -> Context {
        Context::new(Task {
            description: "do a thing".into(),
            source: None,
            deadline: None,
        })
    }

    fn rich_portrait(user: &str) -> UserModel {
        let mut obs = vec![
            Observation::Constraint {
                mode: ConstraintKind::Never,
                rule: "suggest switching to Kubernetes".into(),
                scope: None,
                confidence: 0.9,
            },
            Observation::Communication {
                field: CommField::Language,
                value: "zh-CN".into(),
                confidence: 0.9,
            },
            Observation::Identity {
                field: IdentityField::Role,
                value: "solo founder".into(),
                confidence: 0.85,
            },
            Observation::Goal {
                title: "ship the multi-tenant server".into(),
                status: GoalStatus::Active,
                detail: None,
                confidence: 0.8,
            },
        ];
        for i in 0..10 {
            obs.push(Observation::Relationship {
                name: format!("colleague number {i}"),
                relation: format!("collaborator on the {i}th side project"),
                note: None,
                confidence: 0.85 - (i as f32) * 0.05,
            });
        }
        let mut m = UserModel::new(UserId::new(user));
        m.merge(
            &UserModelDelta {
                observations: obs,
                ..Default::default()
            },
            DAY,
            &PortraitPolicy::default(),
        );
        m
    }

    async fn guide_for(user: &str, mem: Arc<dyn Memory>, budget: usize) -> UserModelGuide {
        let store = Arc::new(UserModelStore::new(mem));
        store.save(&rich_portrait(user)).await.unwrap();
        UserModelGuide::new(store, UserId::new(user))
            .with_policy(PortraitPolicy {
                render_budget_chars: budget,
                ..PortraitPolicy::default()
            })
            .with_now_ms(DAY)
    }

    #[tokio::test]
    async fn injects_a_marked_block_within_budget() {
        let mem: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let g = guide_for("alice", mem, 400).await;
        let mut c = ctx();
        let w = crate::default_world(".");
        g.apply(&mut c, &w).await.unwrap();

        assert_eq!(c.guides.len(), 1);
        let Block::Text(t) = &c.guides[0] else {
            panic!("expected a text block")
        };
        assert!(t.starts_with(MARKER));
        assert!(
            t.chars().count() <= 400,
            "budget blown: {} chars\n{t}",
            t.chars().count()
        );
        // Top tiers survive the squeeze; the tail does not.
        assert!(t.contains("never suggest switching to Kubernetes"), "{t}");
        assert!(t.contains("reply in zh-CN"), "{t}");
        assert!(!t.contains("colleague number 9"), "{t}");
    }

    #[tokio::test]
    async fn re_injection_replaces_instead_of_accumulating() {
        let mem: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let g = guide_for("alice", mem, 600).await;
        let mut c = ctx();
        let w = crate::default_world(".");
        g.apply(&mut c, &w).await.unwrap();
        let first_len = c.guides.len();
        for _ in 0..20 {
            g.apply_before_iter(&mut c, &w).await.unwrap();
        }
        assert_eq!(c.guides.len(), first_len, "20 iterations, still one block");
        let total: usize = c
            .guides
            .iter()
            .map(|b| match b {
                Block::Text(t) => t.chars().count(),
                _ => 0,
            })
            .sum();
        assert!(total <= 600);
    }

    #[tokio::test]
    async fn a_foreign_guide_block_is_left_alone() {
        let mem: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let g = guide_for("alice", mem, 600).await;
        let mut c = ctx();
        c.guides
            .push(Block::Text("[experience-recall]\nstuff".into()));
        let w = crate::default_world(".");
        g.apply(&mut c, &w).await.unwrap();
        g.apply_before_iter(&mut c, &w).await.unwrap();
        assert_eq!(c.guides.len(), 2, "we only strip our own marker");
    }

    #[tokio::test]
    async fn two_users_get_two_different_prompts() {
        let mem: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let store = Arc::new(UserModelStore::new(mem));

        let mut alice = UserModel::new(UserId::new("alice"));
        alice.merge(
            &UserModelDelta {
                observations: vec![Observation::Constraint {
                    mode: ConstraintKind::Never,
                    rule: "suggest Kubernetes".into(),
                    scope: None,
                    confidence: 0.9,
                }],
                ..Default::default()
            },
            DAY,
            &PortraitPolicy::default(),
        );
        let mut bob = UserModel::new(UserId::new("bob"));
        bob.merge(
            &UserModelDelta {
                observations: vec![Observation::Constraint {
                    mode: ConstraintKind::Always,
                    rule: "write tests first".into(),
                    scope: None,
                    confidence: 0.9,
                }],
                ..Default::default()
            },
            DAY,
            &PortraitPolicy::default(),
        );
        store.save(&alice).await.unwrap();
        store.save(&bob).await.unwrap();

        let ga = UserModelGuide::new(store.clone(), UserId::new("alice")).with_now_ms(DAY);
        let gb = UserModelGuide::new(store.clone(), UserId::new("bob")).with_now_ms(DAY);
        let a = ga.block().await.unwrap();
        let b = gb.block().await.unwrap();

        assert!(
            a.contains("Kubernetes") && !a.contains("write tests first"),
            "{a}"
        );
        assert!(
            b.contains("write tests first") && !b.contains("Kubernetes"),
            "{b}"
        );
        assert_ne!(ga.id(), gb.id(), "guide ids are per user");
    }

    #[tokio::test]
    async fn an_unknown_user_injects_nothing() {
        let mem: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let store = Arc::new(UserModelStore::new(mem));
        let g = UserModelGuide::new(store, UserId::new("nobody"));
        let mut c = ctx();
        let w = crate::default_world(".");
        g.apply(&mut c, &w).await.unwrap();
        assert!(c.guides.is_empty(), "no portrait, no tokens spent");
    }

    #[tokio::test]
    async fn invalidate_picks_up_a_new_revision() {
        let mem: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let store = Arc::new(UserModelStore::new(mem));
        let mut m = UserModel::new(UserId::new("alice"));
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Identity {
                    field: IdentityField::Role,
                    value: "solo founder".into(),
                    confidence: 0.9,
                }],
                ..Default::default()
            },
            DAY,
            &PortraitPolicy::default(),
        );
        store.save(&m).await.unwrap();
        let g = UserModelGuide::new(store.clone(), UserId::new("alice")).with_now_ms(DAY);
        assert!(g.block().await.unwrap().contains("solo founder"));

        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Identity {
                    field: IdentityField::Role,
                    value: "engineering manager".into(),
                    confidence: 0.95,
                }],
                ..Default::default()
            },
            DAY * 2,
            &PortraitPolicy::default(),
        );
        store.save(&m).await.unwrap();
        assert!(
            g.block().await.unwrap().contains("solo founder"),
            "cached within the session"
        );
        g.invalidate();
        assert!(g.block().await.unwrap().contains("engineering manager"));
    }
}
