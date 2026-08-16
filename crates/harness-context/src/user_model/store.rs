//! Persistence for [`UserModel`] over the existing [`Memory`] trait.
//!
//! Storing the portrait as a `MemoryEntry` — JSON in `content`, marker + tenant
//! tags on the side — buys every backend at once: `FileMemory`'s JSONL today, a
//! CortexDB or SQLite `Memory` tomorrow, with no new trait to implement and no
//! second storage config for operators to get wrong.
//!
//! Two consequences of that choice are worth stating outright.
//!
//! **Append-only is assumed.** `Memory` has no update and no delete, so a save
//! writes a *new* record and [`UserModelStore::load`] picks the one with the
//! highest `revision`. Old revisions are harmless history (and a free audit
//! trail); a janitor can compact them out of band.
//!
//! **The tenant boundary is enforced here, not in the backend.** Recall is a
//! relevance query, not an access-control mechanism: a semantic backend can and
//! will return another user's row for a similar-looking query. So `load`
//! re-checks `user_id` on the *deserialised* record and discards anything that
//! does not match exactly. Tags and query tokens are only there to make the
//! right record findable.
//!
//! Do not put this behind [`crate::GuardedMemory`]: its dedup would see two
//! consecutive portrait revisions as near-identical and silently drop the newer
//! one. Wrap the raw backend, or the store's writes, not both.

use super::{PortraitPolicy, UserId, UserModel, UserModelError};
use harness_core::{Memory, MemoryEntry};
use std::sync::Arc;

/// Tag marking a memory entry as a stored portrait.
pub const USER_MODEL_TAG: &str = "user-model";

/// Loads and saves per-user portraits through any [`Memory`] backend.
pub struct UserModelStore {
    memory: Arc<dyn Memory>,
    tag: String,
    /// How many candidates to pull before tenant-filtering. Generous because a
    /// keyword backend scores every user's portrait on the shared marker
    /// tokens; the per-user token breaks the tie, but only if the row is in
    /// the candidate set at all.
    fetch_k: usize,
}

impl UserModelStore {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            tag: USER_MODEL_TAG.into(),
            fetch_k: 64,
        }
    }

    /// Override the marker tag — lets two independent portrait sets share one
    /// memory backend (e.g. a work agent and a personal one).
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = tag.into();
        self
    }

    pub fn with_fetch_k(mut self, k: usize) -> Self {
        self.fetch_k = k.max(1);
        self
    }

    fn query_for(&self, user: &UserId) -> String {
        // Three tokens: the marker (finds portraits), the sanitised key
        // (human-greppable), and the hash token (survives backends that drop
        // short tokens, and is unique per raw id).
        format!("{} {} {}", self.tag, user.key(), user.recall_token())
    }

    /// Load `user`'s portrait, or an empty one if nothing is stored yet.
    ///
    /// Never returns another user's data: records are filtered on the
    /// deserialised `user_id`, so even a backend that ignores the query
    /// entirely and returns everything cannot leak across tenants.
    pub async fn load(&self, user: &UserId) -> Result<UserModel, UserModelError> {
        let hits = self
            .memory
            .recall(&self.query_for(user), self.fetch_k)
            .await?;
        let mut best: Option<UserModel> = None;
        for e in hits {
            if !e.tags.iter().any(|t| t == &self.tag) {
                continue;
            }
            let m: UserModel = match serde_json::from_str(&e.content) {
                Ok(m) => m,
                Err(err) => {
                    // A portrait we cannot parse is a portrait we must not
                    // guess at. Skip it and let an older revision win.
                    tracing::warn!(error = %err, id = %e.id, "user model: unparsable record skipped");
                    continue;
                }
            };
            if m.user_id != *user {
                continue;
            }
            if m.schema_version > super::SCHEMA_VERSION {
                tracing::warn!(
                    found = m.schema_version,
                    known = super::SCHEMA_VERSION,
                    "user model: record from a newer schema skipped"
                );
                continue;
            }
            if best.as_ref().is_none_or(|b| m.revision > b.revision) {
                best = Some(m);
            }
        }
        Ok(best.unwrap_or_else(|| UserModel::new(user.clone())))
    }

    /// Persist a portrait. Writes a new revision; see the module docs on
    /// append-only backends.
    pub async fn save(&self, model: &UserModel) -> Result<(), UserModelError> {
        let content = serde_json::to_string(model)?;
        let user = &model.user_id;
        let entry = MemoryEntry::new(content)
            .with_source(format!("{}:{}", self.tag, user.key()))
            .with_tags([
                self.tag.clone(),
                user.tag(),
                user.recall_token(),
                format!("rev:{}", model.revision),
            ]);
        self.memory.write(entry).await?;
        Ok(())
    }

    /// Load, prune to policy, and save back if anything aged out. Cheap enough
    /// to run on a schedule; it makes no model call.
    pub async fn vacuum(
        &self,
        user: &UserId,
        now_ms: i64,
        policy: &PortraitPolicy,
    ) -> Result<u32, UserModelError> {
        let mut m = self.load(user).await?;
        let n = m.prune(now_ms, policy);
        if n > 0 {
            m.revision = m.revision.saturating_add(1);
            m.updated_ms = now_ms;
            self.save(&m).await?;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        ConstraintKind, IdentityField, Observation, PortraitPolicy, UserModelDelta,
    };
    use super::*;
    use async_trait::async_trait;
    use harness_core::MemoryError;
    use std::sync::Mutex;

    const DAY: i64 = 86_400_000;

    /// A deliberately hostile backend: it ignores the query and hands back
    /// EVERY entry, which is what a badly-tuned semantic store effectively
    /// does. If tenancy holds here it holds anywhere.
    #[derive(Default)]
    struct LeakyMemory(Mutex<Vec<MemoryEntry>>);

    #[async_trait]
    impl Memory for LeakyMemory {
        async fn recall(&self, _q: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
            Ok(self.0.lock().unwrap().iter().take(k).cloned().collect())
        }
        async fn write(&self, e: MemoryEntry) -> Result<(), MemoryError> {
            self.0.lock().unwrap().push(e);
            Ok(())
        }
    }

    fn portrait(user: &str, role: &str) -> UserModel {
        let mut m = UserModel::new(UserId::new(user));
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Identity {
                    field: IdentityField::Role,
                    value: role.into(),
                    confidence: 0.9,
                }],
                ..Default::default()
            },
            DAY,
            &PortraitPolicy::default(),
        );
        m
    }

    #[tokio::test]
    async fn round_trips_through_the_memory_trait() {
        let mem: Arc<dyn Memory> = Arc::new(LeakyMemory::default());
        let store = UserModelStore::new(mem);
        let mut m = portrait("alice", "staff engineer");
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Constraint {
                    mode: ConstraintKind::Never,
                    rule: "suggest Kubernetes".into(),
                    scope: None,
                    confidence: 0.9,
                }],
                ..Default::default()
            },
            DAY * 2,
            &PortraitPolicy::default(),
        );

        store.save(&m).await.unwrap();
        let back = store.load(&UserId::new("alice")).await.unwrap();
        assert_eq!(back, m, "portrait survives the Memory round trip intact");
    }

    #[tokio::test]
    async fn two_users_never_see_each_others_portrait() {
        let mem: Arc<dyn Memory> = Arc::new(LeakyMemory::default());
        let store = UserModelStore::new(mem);
        store
            .save(&portrait("alice", "staff engineer"))
            .await
            .unwrap();
        store
            .save(&portrait("bob", "product manager"))
            .await
            .unwrap();

        let a = store.load(&UserId::new("alice")).await.unwrap();
        let b = store.load(&UserId::new("bob")).await.unwrap();
        assert_eq!(a.identity.role.as_ref().unwrap().value, "staff engineer");
        assert_eq!(b.identity.role.as_ref().unwrap().value, "product manager");

        // An id nobody stored gets a blank portrait, not somebody else's.
        let c = store.load(&UserId::new("carol")).await.unwrap();
        assert!(c.is_empty());
        assert_eq!(c.user_id, UserId::new("carol"));
    }

    #[tokio::test]
    async fn load_picks_the_highest_revision_on_append_only_storage() {
        let mem: Arc<dyn Memory> = Arc::new(LeakyMemory::default());
        let store = UserModelStore::new(mem);
        let mut m = portrait("alice", "staff engineer");
        store.save(&m).await.unwrap();
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Identity {
                    field: IdentityField::Role,
                    value: "engineering manager".into(),
                    confidence: 0.95,
                }],
                ..Default::default()
            },
            DAY * 400,
            &PortraitPolicy::default(),
        );
        store.save(&m).await.unwrap();

        let back = store.load(&UserId::new("alice")).await.unwrap();
        assert_eq!(back.revision, m.revision);
        assert_eq!(
            back.identity.role.as_ref().unwrap().value,
            "engineering manager"
        );
    }

    #[tokio::test]
    async fn round_trips_through_real_file_memory() {
        // The contract test that matters: the shipped JSONL backend, not a stub.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "harness-user-model-{}-{nanos}.jsonl",
            std::process::id()
        ));
        let mem: Arc<dyn Memory> = Arc::new(crate::FileMemory::open(&path).unwrap());
        let store = UserModelStore::new(mem);

        // Short ids tokenise away in keyword backends; `recall_token` is what
        // keeps them findable.
        store.save(&portrait("a1", "founder")).await.unwrap();
        store.save(&portrait("b2", "designer")).await.unwrap();

        let a = store.load(&UserId::new("a1")).await.unwrap();
        assert_eq!(a.identity.role.as_ref().unwrap().value, "founder");
        let b = store.load(&UserId::new("b2")).await.unwrap();
        assert_eq!(b.identity.role.as_ref().unwrap().value, "designer");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn vacuum_ages_out_stale_items() {
        let mem: Arc<dyn Memory> = Arc::new(LeakyMemory::default());
        let store = UserModelStore::new(mem);
        let mut m = UserModel::new(UserId::new("alice"));
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Goal {
                    title: "ship the beta".into(),
                    status: super::super::GoalStatus::Active,
                    detail: None,
                    confidence: 0.8,
                }],
                ..Default::default()
            },
            0,
            &PortraitPolicy::default(),
        );
        store.save(&m).await.unwrap();

        let n = store
            .vacuum(&UserId::new("alice"), DAY * 365, &PortraitPolicy::default())
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(
            store
                .load(&UserId::new("alice"))
                .await
                .unwrap()
                .goals
                .is_empty()
        );
    }
}
