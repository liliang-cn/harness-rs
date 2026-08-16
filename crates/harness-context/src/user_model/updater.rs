//! Deepening the portrait: one LLM call produces a *proposal*, Rust merges it.
//!
//! # Why the model never writes the portrait
//!
//! The obvious design — hand the model the current portrait and ask for the new
//! one — fails in three ways that show up only after weeks of use: the model
//! silently drops fields it did not find interesting, it rewrites wording so
//! nothing is stable enough to diff or dedupe, and it has no notion of "this
//! contradicts what you told me in March" beyond whatever it feels like saying.
//! So the model's entire job here is to *notice* ([`Observation`]s, each with a
//! confidence), and [`UserModel::merge`] decides what that does to the stored
//! belief. The worst a hallucinating model can do is add a low-confidence claim
//! that ages out.
//!
//! # Why this must not run every turn
//!
//! An update round costs a full extra model call whose input is the portrait
//! plus a slice of transcript — call it 2-4k tokens, roughly the same order as
//! the turn that triggered it. Running it per turn therefore *doubles* the cost
//! of the whole agent. What it buys is close to nothing: people do not reveal a
//! new standing preference every turn, and the merge rules are explicitly built
//! so that late-arriving evidence still lands correctly (recency-dominant
//! conflict resolution means a fact learned at turn 30 instead of turn 12 wins
//! just the same).
//!
//! So updates are event-driven, through [`UpdatePolicy`] and [`UpdateTracker`]:
//!
//! - **evidence budget** — enough new user text has accumulated to be worth
//!   reading (default 4000 chars, ~1k tokens). This is the trigger that
//!   actually fires most of the time, because it tracks information volume
//!   rather than turn count; ten one-word turns are not worth a call.
//! - **turn count** — a backstop for long conversations of short messages
//!   (default every 12 turns).
//! - **session end** — the highest-yield single moment, because the whole
//!   session is available as evidence and nobody is waiting on the latency.
//!
//! plus a **rate limit** (`min_interval_ms`, default 5 minutes) that no trigger
//! can bypass, and a hard requirement that at least one turn has happened since
//! the last update, so an idle session-end cannot burn a call.
//!
//! Defaults land around 1-3 calls per hour-long session. To disable updating
//! entirely, set every trigger off — the guide keeps rendering whatever is
//! already stored.

use super::{
    Evidence, MergeReport, PortraitPolicy, UserId, UserModel, UserModelDelta, UserModelError,
    UserModelStore,
};
use harness_core::{Block, Context, Model, ResponseFormat, Task, Turn, TurnRole};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// trigger policy
// ---------------------------------------------------------------------------

/// When to spend a model call on deepening the portrait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatePolicy {
    /// Fire after this many turns since the last update. `None` disables.
    pub every_n_turns: Option<u32>,
    /// Fire when the session ends (if any turn happened since the last update).
    pub on_session_end: bool,
    /// Fire once this many chars of new user text have accumulated. `None`
    /// disables.
    pub min_new_chars: Option<usize>,
    /// Floor between two updates for the same user; no trigger overrides it.
    /// Guards against a burst of long messages billing a call per turn.
    pub min_interval_ms: i64,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            every_n_turns: Some(12),
            on_session_end: true,
            min_new_chars: Some(4000),
            min_interval_ms: 5 * 60 * 1000,
        }
    }
}

impl UpdatePolicy {
    /// Never update automatically — the caller drives [`UserModelUpdater`]
    /// itself (a nightly batch job, an explicit "remember this about me").
    pub fn manual() -> Self {
        Self {
            every_n_turns: None,
            on_session_end: false,
            min_new_chars: None,
            min_interval_ms: 0,
        }
    }
}

/// Why an update fired. Logged, and handy in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    EvidenceBudget,
    TurnCount,
    SessionEnd,
}

/// [`UpdateTracker`]'s counters, serialisable so a host that does not stay
/// resident can persist them between turns. See [`UpdateTracker::state`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateTrackerState {
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub chars: usize,
    #[serde(default)]
    pub last_update_ms: i64,
}

/// Per-user counters feeding [`UpdatePolicy`]. One tracker per user id —
/// there is no global "turns since update", because in a multi-tenant server
/// that number would be meaningless.
#[derive(Debug, Clone)]
pub struct UpdateTracker {
    user: UserId,
    turns: u32,
    chars: usize,
    last_update_ms: i64,
    session_ended: bool,
}

impl UpdateTracker {
    pub fn new(user: UserId) -> Self {
        Self {
            user,
            turns: 0,
            chars: 0,
            last_update_ms: 0,
            session_ended: false,
        }
    }

    /// Resume a tracker that already updated at `ms` (e.g. from the loaded
    /// portrait's `updated_ms`), so a server restart does not hand every user
    /// a free immediate update.
    pub fn since(mut self, last_update_ms: i64) -> Self {
        self.last_update_ms = last_update_ms;
        self
    }

    pub fn user(&self) -> &UserId {
        &self.user
    }

    /// The counters, in a form a host can persist and hand back.
    ///
    /// A tracker held only in process memory is correct for a long-lived
    /// server and silently wrong everywhere else: a CLI, a serverless handler
    /// or a worker pool starts a fresh process per turn, so `turns` resets to
    /// 1 on every message and an "every N turns" trigger never reaches N. The
    /// failure mode is the bad kind — nothing errors, the portrait simply
    /// never updates and the feature looks like it is merely not very good.
    /// Hosts that are not one long-lived process must round-trip this.
    pub fn state(&self) -> UpdateTrackerState {
        UpdateTrackerState {
            turns: self.turns,
            chars: self.chars,
            last_update_ms: self.last_update_ms,
        }
    }

    /// Rebuild a tracker from persisted counters. `session_ended` is
    /// deliberately not carried: it describes *this* process's session, and
    /// resuming into a new one with it set would fire an immediate update.
    pub fn from_state(user: UserId, st: UpdateTrackerState) -> Self {
        Self {
            user,
            turns: st.turns,
            chars: st.chars,
            last_update_ms: st.last_update_ms,
            session_ended: false,
        }
    }

    /// Record one user turn. `user_text_chars` is the length of what the *user*
    /// said — assistant and tool output are not evidence about the user, and
    /// counting them would make a verbose agent trigger its own updates.
    pub fn record_turn(&mut self, user_text_chars: usize) {
        self.turns = self.turns.saturating_add(1);
        self.chars = self.chars.saturating_add(user_text_chars);
    }

    pub fn note_session_end(&mut self) {
        self.session_ended = true;
    }

    /// The first trigger that fires, or `None`. Priority is
    /// evidence > turns > session end: when several are true at once, report
    /// the one that says most about *why* there is something to learn.
    pub fn due(&self, policy: &UpdatePolicy, now_ms: i64) -> Option<TriggerReason> {
        if self.turns == 0 {
            return None;
        }
        if now_ms.saturating_sub(self.last_update_ms) < policy.min_interval_ms {
            return None;
        }
        if policy.min_new_chars.is_some_and(|n| self.chars >= n) {
            return Some(TriggerReason::EvidenceBudget);
        }
        if policy
            .every_n_turns
            .is_some_and(|n| n > 0 && self.turns >= n)
        {
            return Some(TriggerReason::TurnCount);
        }
        if policy.on_session_end && self.session_ended {
            return Some(TriggerReason::SessionEnd);
        }
        None
    }

    /// Reset the counters after a successful update.
    pub fn mark_updated(&mut self, now_ms: i64) {
        self.turns = 0;
        self.chars = 0;
        self.session_ended = false;
        self.last_update_ms = now_ms;
    }

    pub fn turns_since_update(&self) -> u32 {
        self.turns
    }

    pub fn chars_since_update(&self) -> usize {
        self.chars
    }
}

/// Trackers for many users at once — the shape a multi-tenant server needs.
/// Keyed by [`UserId`], so there is no way to record user A's turn against
/// user B's counters by passing the wrong `&str`.
#[derive(Default)]
pub struct UpdateTrackers {
    inner: std::sync::Mutex<std::collections::HashMap<UserId, UpdateTracker>>,
}

impl UpdateTrackers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mutate (creating on first sight) the tracker for `user`.
    pub fn with<R>(&self, user: &UserId, f: impl FnOnce(&mut UpdateTracker) -> R) -> R {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let t = g
            .entry(user.clone())
            .or_insert_with(|| UpdateTracker::new(user.clone()));
        f(t)
    }

    pub fn record_turn(&self, user: &UserId, user_text_chars: usize) {
        self.with(user, |t| t.record_turn(user_text_chars));
    }

    pub fn note_session_end(&self, user: &UserId) {
        self.with(user, |t| t.note_session_end());
    }

    pub fn due(&self, user: &UserId, policy: &UpdatePolicy, now_ms: i64) -> Option<TriggerReason> {
        self.with(user, |t| t.due(policy, now_ms))
    }

    pub fn mark_updated(&self, user: &UserId, now_ms: i64) {
        self.with(user, |t| t.mark_updated(now_ms));
    }

    /// Drop a user's counters (session closed, tenant removed).
    pub fn forget(&self, user: &UserId) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(user);
    }
}

// ---------------------------------------------------------------------------
// updater
// ---------------------------------------------------------------------------

const SYSTEM: &str = "\
You maintain a long-term portrait of ONE user for an AI assistant.
You are given the CURRENT PORTRAIT and a slice of RECENT CONVERSATION.
Report only what the conversation newly reveals about the user. Reply with JSON only.

{\"observations\": [ ... ], \"resolved_questions\": [\"<id>\"], \"retracted\": [\"<id>\"]}

Each observation is one of:
{\"kind\":\"identity\",\"field\":\"display_name|role|org|locale|timezone\",\"value\":\"...\",\"confidence\":0.7}
{\"kind\":\"communication\",\"field\":\"language\",\"value\":\"zh-CN\",\"confidence\":0.7}
{\"kind\":\"communication\",\"field\":\"verbosity\",\"value\":\"terse|balanced|thorough\",\"confidence\":0.7}
{\"kind\":\"communication\",\"field\":\"formality\",\"value\":\"casual|neutral|formal\",\"confidence\":0.7}
{\"kind\":\"style_note\",\"text\":\"no emoji\",\"confidence\":0.7}
{\"kind\":\"expertise\",\"domain\":\"rust\",\"level\":\"novice|learning|competent|expert\",\"note\":null,\"confidence\":0.7}
{\"kind\":\"constraint\",\"mode\":\"never|always|avoid\",\"rule\":\"suggest Kubernetes\",\"scope\":null,\"confidence\":0.9}
{\"kind\":\"goal\",\"title\":\"ship the multi-tenant server\",\"status\":\"active|paused|done|abandoned\",\"detail\":null,\"confidence\":0.8}
{\"kind\":\"relationship\",\"name\":\"Wei\",\"relation\":\"co-founder\",\"note\":null,\"confidence\":0.7}
{\"kind\":\"open_question\",\"question\":\"which timezone should scheduling assume?\",\"why\":null,\"confidence\":0.5}

Rules:
- Evidence only. Do not restate the current portrait, and do not infer from your own replies.
- confidence: 0.9 the user said it outright, 0.6 strongly implied, 0.3 a guess. Never 1.0.
- If the conversation contradicts the portrait, emit the NEW value as a normal observation.
  Do not try to resolve the conflict; the merge code does that.
- `retracted` is only for an explicit \"forget that\" from the user.
- An empty observations list is a good answer when nothing new was revealed.
- `constraint` rules are bare predicates without the leading never/always.
- No prose, no markdown fences, no explanation. JSON object only.";

/// Turns recent conversation into a [`UserModelDelta`] with one model call.
pub struct UserModelUpdater {
    model: Arc<dyn Model>,
    policy: PortraitPolicy,
    /// Cap on the portrait summary we feed back in. The prompt only needs
    /// enough context to avoid re-reporting known facts.
    portrait_prompt_chars: usize,
}

impl UserModelUpdater {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            policy: PortraitPolicy::default(),
            portrait_prompt_chars: 2000,
        }
    }

    pub fn with_policy(mut self, policy: PortraitPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_portrait_prompt_chars(mut self, n: usize) -> Self {
        self.portrait_prompt_chars = n;
        self
    }

    pub fn policy(&self) -> &PortraitPolicy {
        &self.policy
    }

    /// One model call. Returns what the model *proposed*; merging is the
    /// caller's (deterministic) business — see [`UserModelUpdater::refresh`].
    pub async fn propose(
        &self,
        current: &UserModel,
        evidence: &Evidence,
    ) -> Result<UserModelDelta, UserModelError> {
        // The scope check is here, not at the call site, because "which user is
        // this evidence about" is exactly the thing a multi-tenant server gets
        // wrong under concurrency.
        if current.user_id != evidence.user {
            return Err(UserModelError::Scope {
                portrait: current.user_id.to_string(),
                evidence: evidence.user.to_string(),
            });
        }
        if evidence.transcript.trim().is_empty() {
            return Ok(UserModelDelta::default());
        }

        let known = current
            .render_within(evidence.now_ms, &self.policy, self.portrait_prompt_chars)
            .unwrap_or_else(|| "(nothing known yet)".into());

        let mut ctx = Context::new(Task {
            description: "Update the long-term user portrait from recent conversation.".into(),
            source: Some("user-model-updater".into()),
            deadline: None,
        });
        ctx.system.push(Block::Text(SYSTEM.into()));
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(format!(
                "CURRENT PORTRAIT:\n{known}\n\nRECENT CONVERSATION:\n{}",
                evidence.transcript
            ))],
        });
        // Ask providers for JSON natively where they support it; the parser
        // below still tolerates a fenced or chatty answer from those that don't.
        ctx.response_format = ResponseFormat::JsonObject;

        let out = self.model.complete(&ctx).await?;
        let text = out.text.unwrap_or_default();
        let json = extract_json_object(&text).ok_or_else(|| {
            UserModelError::Parse(format!("no JSON object in reply: {text:.200}"))
        })?;
        let mut delta: UserModelDelta = serde_json::from_str(json)
            .map_err(|e| UserModelError::Parse(format!("{e}: {json:.200}")))?;
        if delta.source.is_none() {
            delta.source = evidence.source.clone();
        }
        Ok(delta)
    }

    /// Propose, then merge deterministically into `current`. The portrait is
    /// left untouched if the call or the parse fails — a failed update must
    /// never corrupt what is already known.
    pub async fn refresh(
        &self,
        current: &mut UserModel,
        evidence: &Evidence,
    ) -> Result<MergeReport, UserModelError> {
        let delta = self.propose(current, evidence).await?;
        Ok(current.merge(&delta, evidence.now_ms, &self.policy))
    }

    /// Load, refresh, and save back through a [`UserModelStore`] — the whole
    /// round for one user. Saves only when the merge changed something, so a
    /// "nothing new" round costs no write.
    pub async fn refresh_stored(
        &self,
        store: &UserModelStore,
        user: &UserId,
        evidence: &Evidence,
    ) -> Result<(UserModel, MergeReport), UserModelError> {
        if *user != evidence.user {
            return Err(UserModelError::Scope {
                portrait: user.to_string(),
                evidence: evidence.user.to_string(),
            });
        }
        let mut model = store.load(user).await?;
        let report = self.refresh(&mut model, evidence).await?;
        if report.changed() {
            store.save(&model).await?;
        }
        Ok((model, report))
    }
}

/// Pull the outermost JSON object out of a model reply, tolerating ```json
/// fences and leading prose. Cheaper and far more robust than asking the model
/// again.
fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(&text[start..=end])
}

#[cfg(test)]
mod tests {
    use super::super::{CommField, ConstraintKind, Observation, UserModelStore};
    use super::*;
    use async_trait::async_trait;
    use harness_core::{Memory, MemoryEntry, MemoryError, ModelError, ModelInfo, ModelOutput};
    use std::sync::Mutex;

    const DAY: i64 = 86_400_000;

    fn mi() -> ModelInfo {
        ModelInfo {
            handle: "stub".into(),
            provider: "test".into(),
            model: "stub".into(),
            context_window: 8192,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: false,
            supports_streaming: false,
            supports_web_grounding: false,
        }
    }

    /// Same shape as the stub models in `harness-loop`'s tests: a scripted
    /// reply plus a record of what it was asked.
    struct StubModel {
        reply: String,
        seen: Mutex<Vec<String>>,
        fail: bool,
    }

    impl StubModel {
        fn new(reply: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.into(),
                seen: Mutex::new(Vec::new()),
                fail: false,
            })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self {
                reply: String::new(),
                seen: Mutex::new(Vec::new()),
                fail: true,
            })
        }
        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Model for StubModel {
        async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
            let mut sent = String::new();
            for t in &ctx.history {
                for b in &t.blocks {
                    if let Block::Text(s) = b {
                        sent.push_str(s);
                    }
                }
            }
            self.seen.lock().unwrap().push(sent);
            if self.fail {
                return Err(ModelError::Transport("boom".into()));
            }
            Ok(ModelOutput {
                text: Some(self.reply.clone()),
                ..Default::default()
            })
        }
        fn info(&self) -> ModelInfo {
            mi()
        }
    }

    #[derive(Default)]
    struct VecMemory(Mutex<Vec<MemoryEntry>>);
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

    fn evidence(user: &str, text: &str, now_ms: i64) -> Evidence {
        Evidence::new(UserId::new(user), text).at(now_ms)
    }

    // ---- trigger policy -------------------------------------------------

    #[test]
    fn does_not_fire_before_the_turn_threshold() {
        let policy = UpdatePolicy {
            every_n_turns: Some(3),
            on_session_end: false,
            min_new_chars: None,
            min_interval_ms: 0,
        };
        let mut t = UpdateTracker::new(UserId::new("u1"));
        assert_eq!(t.due(&policy, 1000), None, "no turns, nothing to learn");
        t.record_turn(10);
        assert_eq!(t.due(&policy, 1000), None);
        t.record_turn(10);
        assert_eq!(t.due(&policy, 1000), None);
        t.record_turn(10);
        assert_eq!(t.due(&policy, 1000), Some(TriggerReason::TurnCount));

        t.mark_updated(1000);
        assert_eq!(t.due(&policy, 2000), None, "counters reset after an update");
    }

    #[test]
    fn evidence_budget_fires_before_the_turn_count() {
        let policy = UpdatePolicy {
            min_interval_ms: 0,
            ..UpdatePolicy::default()
        };
        let mut t = UpdateTracker::new(UserId::new("u1"));
        t.record_turn(3999);
        assert_eq!(t.due(&policy, 0), None);
        t.record_turn(1);
        assert_eq!(t.due(&policy, 0), Some(TriggerReason::EvidenceBudget));
    }

    #[test]
    fn session_end_fires_only_with_new_turns() {
        let policy = UpdatePolicy {
            min_interval_ms: 0,
            ..UpdatePolicy::default()
        };
        let mut t = UpdateTracker::new(UserId::new("u1"));
        t.note_session_end();
        assert_eq!(
            t.due(&policy, 0),
            None,
            "idle session end must not bill a call"
        );
        t.record_turn(5);
        assert_eq!(t.due(&policy, 0), Some(TriggerReason::SessionEnd));
    }

    #[test]
    fn the_rate_limit_overrides_every_trigger() {
        let policy = UpdatePolicy::default(); // 5 min floor
        let mut t = UpdateTracker::new(UserId::new("u1")).since(1_000_000);
        t.record_turn(100_000);
        t.note_session_end();
        assert_eq!(t.due(&policy, 1_000_000 + 60_000), None);
        assert_eq!(
            t.due(&policy, 1_000_000 + 5 * 60_000),
            Some(TriggerReason::EvidenceBudget)
        );
    }

    #[test]
    fn manual_policy_never_fires() {
        let mut t = UpdateTracker::new(UserId::new("u1"));
        t.record_turn(100_000);
        t.note_session_end();
        assert_eq!(t.due(&UpdatePolicy::manual(), i64::MAX / 2), None);
    }

    #[test]
    fn trackers_are_per_user() {
        let trackers = UpdateTrackers::new();
        let a = UserId::new("alice");
        let b = UserId::new("bob");
        let policy = UpdatePolicy {
            every_n_turns: Some(2),
            on_session_end: false,
            min_new_chars: None,
            min_interval_ms: 0,
        };
        trackers.record_turn(&a, 10);
        trackers.record_turn(&a, 10);
        assert_eq!(trackers.due(&a, &policy, 0), Some(TriggerReason::TurnCount));
        assert_eq!(
            trackers.due(&b, &policy, 0),
            None,
            "bob's counters are bob's"
        );
    }

    // ---- updater --------------------------------------------------------

    #[tokio::test]
    async fn a_proposal_is_merged_deterministically() {
        let model = StubModel::new(
            r#"Sure! ```json
            {"observations":[
              {"kind":"constraint","mode":"never","rule":"suggest Kubernetes","confidence":0.9},
              {"kind":"communication","field":"language","value":"zh-CN","confidence":0.8}
            ]}
            ```"#,
        );
        let updater = UserModelUpdater::new(model.clone());
        let mut m = UserModel::new(UserId::new("alice"));
        let report = updater
            .refresh(&mut m, &evidence("alice", "user: 别再提 k8s 了", DAY))
            .await
            .unwrap();

        assert_eq!(model.calls(), 1, "exactly one model call per round");
        assert_eq!(report.added, 2);
        assert_eq!(m.constraints[0].kind, ConstraintKind::Never);
        assert_eq!(m.communication.language.as_ref().unwrap().value, "zh-CN");
        // The prompt carried the transcript, and (being empty) no portrait.
        assert!(model.seen.lock().unwrap()[0].contains("别再提 k8s 了"));
    }

    #[tokio::test]
    async fn a_failed_call_leaves_the_portrait_untouched() {
        let updater = UserModelUpdater::new(StubModel::failing());
        let mut m = UserModel::new(UserId::new("alice"));
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Communication {
                    field: CommField::Verbosity,
                    value: "terse".into(),
                    confidence: 0.8,
                }],
                ..Default::default()
            },
            DAY,
            &PortraitPolicy::default(),
        );
        let before = m.clone();
        let err = updater
            .refresh(&mut m, &evidence("alice", "user: hi", DAY * 2))
            .await
            .unwrap_err();
        assert!(matches!(err, UserModelError::Model(_)), "{err}");
        assert_eq!(m, before);
    }

    #[tokio::test]
    async fn garbage_output_is_a_parse_error_not_a_wipe() {
        let updater = UserModelUpdater::new(StubModel::new("I'd rather not."));
        let mut m = UserModel::new(UserId::new("alice"));
        let err = updater
            .refresh(&mut m, &evidence("alice", "user: hi", DAY))
            .await
            .unwrap_err();
        assert!(matches!(err, UserModelError::Parse(_)), "{err}");
        assert!(m.is_empty());
    }

    #[tokio::test]
    async fn evidence_for_the_wrong_user_is_refused() {
        let updater = UserModelUpdater::new(StubModel::new(r#"{"observations":[]}"#));
        let mut m = UserModel::new(UserId::new("alice"));
        let err = updater
            .refresh(&mut m, &evidence("bob", "user: hi", DAY))
            .await
            .unwrap_err();
        assert!(matches!(err, UserModelError::Scope { .. }), "{err}");
    }

    #[tokio::test]
    async fn refresh_stored_persists_only_when_something_changed() {
        let mem: Arc<dyn Memory> = Arc::new(VecMemory::default());
        let store = UserModelStore::new(mem.clone());
        let updater = UserModelUpdater::new(StubModel::new(r#"{"observations":[]}"#));
        let user = UserId::new("alice");

        let (m, report) = updater
            .refresh_stored(&store, &user, &evidence("alice", "user: hello", DAY))
            .await
            .unwrap();
        assert!(!report.changed());
        assert!(m.is_empty());

        let updater = UserModelUpdater::new(StubModel::new(
            r#"{"observations":[{"kind":"goal","title":"ship the beta","status":"active","confidence":0.8}]}"#,
        ));
        let (m, report) = updater
            .refresh_stored(&store, &user, &evidence("alice", "user: beta soon", DAY))
            .await
            .unwrap();
        assert_eq!(report.added, 1);
        assert_eq!(m.goals.len(), 1);
        assert_eq!(store.load(&user).await.unwrap().goals.len(), 1);
    }

    #[tokio::test]
    async fn empty_transcript_short_circuits_without_a_call() {
        let model = StubModel::new(r#"{"observations":[]}"#);
        let updater = UserModelUpdater::new(model.clone());
        let m = UserModel::new(UserId::new("alice"));
        let delta = updater
            .propose(&m, &evidence("alice", "   ", DAY))
            .await
            .unwrap();
        assert!(delta.is_empty());
        assert_eq!(model.calls(), 0, "no evidence, no spend");
    }

    /// A host that starts a fresh process per turn must still reach the
    /// trigger. This is the bug the in-memory tracker has by construction:
    /// counters reset on every message, `turns` never passes 1, and an
    /// every-12-turns policy fires never — silently.
    #[test]
    fn counters_survive_a_process_that_does_not() {
        let user = UserId::new("u1");
        let policy = UpdatePolicy {
            every_n_turns: Some(3),
            min_new_chars: None,
            on_session_end: false,
            min_interval_ms: 0,
        };

        // Naive host: a new tracker per turn. Never fires, however long you go.
        for turn in 0..10 {
            let mut t = UpdateTracker::new(user.clone());
            t.record_turn(50);
            assert!(
                t.due(&policy, 1_000 * (turn + 1)).is_none(),
                "a per-turn tracker must never reach the threshold — that is the bug"
            );
        }

        // Same host, round-tripping the state the way a CLI or a serverless
        // handler has to.
        let mut saved = UpdateTrackerState::default();
        let mut fired_on = None;
        for turn in 0..10i64 {
            let mut t = UpdateTracker::from_state(user.clone(), saved);
            t.record_turn(50);
            if fired_on.is_none()
                && let Some(reason) = t.due(&policy, 1_000 * (turn + 1))
            {
                assert_eq!(reason, TriggerReason::TurnCount);
                fired_on = Some(turn + 1);
                t.mark_updated(1_000 * (turn + 1));
            }
            // Persisting is what a JSON file on disk would hold.
            saved = t.state();
            let round_tripped: UpdateTrackerState =
                serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
            assert_eq!(round_tripped, saved, "state must survive the file");
            saved = round_tripped;
        }
        assert_eq!(fired_on, Some(3), "should have fired on the third turn");
    }

    /// Resuming must not carry `session_ended` into the next process, or every
    /// restart hands the user a free update.
    #[test]
    fn a_resumed_tracker_does_not_inherit_a_finished_session() {
        let user = UserId::new("u1");
        let policy = UpdatePolicy {
            every_n_turns: None,
            min_new_chars: None,
            on_session_end: true,
            min_interval_ms: 0,
        };
        let mut t = UpdateTracker::new(user.clone());
        t.record_turn(10);
        t.note_session_end();
        assert!(t.due(&policy, 10_000).is_some());

        let resumed = UpdateTracker::from_state(user, t.state());
        assert!(
            resumed.due(&policy, 20_000).is_none(),
            "session-end is about this process, not the persisted counters"
        );
    }
}
