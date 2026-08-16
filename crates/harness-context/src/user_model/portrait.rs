//! The portrait itself: [`UserModel`], its provenance-carrying field types,
//! the deterministic merge, the aging rules, and the budgeted renderer.
//!
//! Everything here is pure data + pure functions — no `Memory`, no `Model`, no
//! clock. Time is always passed in as `now_ms` so the aging and conflict rules
//! are testable without sleeping.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Bumped whenever a stored portrait can no longer be deserialised by this
/// code. Readers that see a *higher* version than they know must refuse to use
/// the record rather than silently drop fields they cannot represent — a
/// half-understood portrait is worse than none.
pub const SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// tenancy
// ---------------------------------------------------------------------------

/// The tenant key every portrait operation is scoped by.
///
/// A newtype rather than a bare `String` on purpose: the consumer is a
/// multi-tenant server, and "which user is this?" must be a type the compiler
/// can see in every signature, not a convention about argument order. There is
/// deliberately no `Default` and no ambient/global "current user" — you cannot
/// load, render, or update a portrait without naming whose it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(String);

impl UserId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw id exactly as the caller supplied it. This is what equality —
    /// and therefore the tenant boundary — is decided on.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A token-safe rendering used in memory tags and recall queries.
    ///
    /// Never used for the security check (that is `as_str()` equality on the
    /// deserialised record): two distinct ids that sanitise to the same key
    /// would otherwise be a cross-tenant leak. It exists only so that recall
    /// backends which tokenise on non-alphanumerics have something stable to
    /// match on.
    pub fn key(&self) -> String {
        let cleaned: String = self
            .0
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let cleaned = if cleaned.is_empty() {
            "_".to_string()
        } else {
            cleaned
        };
        cleaned.chars().take(48).collect()
    }

    /// A always-long, always-distinctive retrieval token derived from the raw
    /// id. Short ids (`"a1"`) tokenise away entirely in keyword backends that
    /// drop <3-char tokens — a portrait that cannot be recalled is a portrait
    /// that silently resets, so we give every user one token that is long by
    /// construction.
    pub fn recall_token(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.0.hash(&mut h);
        format!("uid{:016x}", h.finish())
    }

    /// Tag written on every stored portrait entry.
    pub fn tag(&self) -> String {
        format!("user:{}", self.key())
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for UserId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for UserId {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

// ---------------------------------------------------------------------------
// provenance
// ---------------------------------------------------------------------------

/// When a belief was learned, how strongly, and whether anything has since
/// disagreed with it.
///
/// Every claim in the portrait carries one. Without provenance a portrait can
/// only ever grow: there is no principled way to decide that "user is learning
/// Rust", asserted once eight months ago, should stop being injected into the
/// prompt. With it, one number — the *effective* confidence, decayed by age —
/// drives both aging-out and the render budget's eviction order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// First time this claim was seen. Never moves; it is the audit trail.
    pub learned_ms: i64,
    /// Last time this claim was reaffirmed or changed. The decay clock.
    pub updated_ms: i64,
    /// Confidence as of `updated_ms`, in `[0, 1]`. Decays from there.
    pub confidence: f32,
    /// How many separate updates have supported this claim. Diagnostics and a
    /// tie-break; the confidence math already folds repetition in.
    pub evidence_count: u32,
    /// Where the supporting evidence came from (session id, "user-stated", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Set once something contradicted this claim — whether the challenger won
    /// or lost. Rendered as a `(?)` so the agent knows not to assert it flatly.
    #[serde(default, skip_serializing_if = "is_false")]
    pub contradicted: bool,
    /// The losing side of the most recent contradiction. Kept so that a merge
    /// never destroys evidence outright: an operator reading the JSON can see
    /// what was overruled and when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Provenance {
    pub fn new(now_ms: i64, confidence: f32, source: Option<String>) -> Self {
        Self {
            learned_ms: now_ms,
            updated_ms: now_ms,
            confidence: clamp_conf(confidence),
            evidence_count: 1,
            source,
            contradicted: false,
            superseded: None,
        }
    }

    /// Confidence discounted for age: `c * 0.5^(age / half_life)`.
    ///
    /// Exponential rather than a cliff because belief staleness is gradual —
    /// a fact learned yesterday and one learned a year ago should not be
    /// treated identically the moment some threshold trips.
    pub fn effective_confidence(&self, now_ms: i64, half_life_days: f32) -> f32 {
        if half_life_days <= 0.0 {
            return self.confidence;
        }
        let age_days = ((now_ms - self.updated_ms).max(0) as f32) / 86_400_000.0;
        self.confidence * 0.5f32.powf(age_days / half_life_days)
    }
}

/// Model-proposed confidences are untrusted input. We clamp rather than reject:
/// a model that answers `1.0` for everything (they do) should not be able to
/// mint an unfalsifiable belief, and a `0.0` should not create a ghost entry
/// that can never be pruned because it is already at the floor.
fn clamp_conf(c: f32) -> f32 {
    if !c.is_finite() {
        return 0.3;
    }
    c.clamp(0.05, 0.95)
}

/// A value plus the provenance of believing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attested<T> {
    pub value: T,
    #[serde(flatten)]
    pub prov: Provenance,
}

impl<T> Attested<T> {
    pub fn new(value: T, now_ms: i64, confidence: f32, source: Option<String>) -> Self {
        Self {
            value,
            prov: Provenance::new(now_ms, confidence, source),
        }
    }
}

// ---------------------------------------------------------------------------
// vocabulary
// ---------------------------------------------------------------------------

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self { $(Self::$variant => $text),+ })
            }
        }

        impl std::str::FromStr for $name {
            type Err = ();
            fn from_str(s: &str) -> Result<Self, ()> {
                match s.trim().to_ascii_lowercase().as_str() {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(()),
                }
            }
        }
    };
}

str_enum! {
    /// How much the user already knows about a domain. Four steps, because the
    /// only decision it drives is how much to explain, and prose like
    /// "intermediate-advanced" does not change that decision.
    ExpertiseLevel {
        Novice => "novice",
        Learning => "learning",
        Competent => "competent",
        Expert => "expert",
    }
}

str_enum! {
    /// Reply length preference.
    Verbosity {
        Terse => "terse",
        Balanced => "balanced",
        Thorough => "thorough",
    }
}

str_enum! {
    /// Register preference.
    Formality {
        Casual => "casual",
        Neutral => "neutral",
        Formal => "formal",
    }
}

str_enum! {
    /// The polarity of a standing constraint.
    ConstraintKind {
        Never => "never",
        Always => "always",
        Avoid => "avoid",
    }
}

str_enum! {
    /// Lifecycle of a recurring goal / project.
    GoalStatus {
        Active => "active",
        Paused => "paused",
        Done => "done",
        Abandoned => "abandoned",
    }
}

str_enum! {
    /// Which identity slot an observation is about.
    IdentityField {
        DisplayName => "display_name",
        Role => "role",
        Org => "org",
        Locale => "locale",
        Timezone => "timezone",
    }
}

str_enum! {
    /// Which communication slot an observation is about.
    CommField {
        Language => "language",
        Verbosity => "verbosity",
        Formality => "formality",
    }
}

// ---------------------------------------------------------------------------
// the portrait
// ---------------------------------------------------------------------------

/// Who the user is, as far as the agent has been able to tell.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<Attested<String>>,
    /// Job / role in their own words: "staff backend engineer", "solo founder".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Attested<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<Attested<String>>,
    /// BCP-47-ish locale, mirroring `harness_core::UserProfile::locale`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<Attested<String>>,
    /// IANA timezone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<Attested<String>>,
}

/// How the user wants to be talked to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommunicationPrefs {
    /// Language to reply in. Distinct from `Identity::locale`: people read one
    /// locale's dates and want replies in another language often enough.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Attested<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<Attested<Verbosity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formality: Option<Attested<Formality>>,
    /// The escape hatch, kept deliberately narrow: short stylistic rules that
    /// do not fit the three slots above ("no emoji", "code first, prose after").
    /// Anything that is really a prohibition belongs in [`Constraint`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<StyleNote>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleNote {
    /// Slug of `text`; the merge key.
    pub id: String,
    pub text: String,
    #[serde(flatten)]
    pub prov: Provenance,
}

/// What the user knows, per domain — the single biggest driver of how much
/// scaffolding a reply needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainExpertise {
    /// Normalised domain slug ("rust", "kubernetes", "german-tax-law").
    pub domain: String,
    pub level: ExpertiseLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(flatten)]
    pub prov: Provenance,
}

/// A standing rule. "Never suggest X again" is the single most expensive thing
/// for an agent to forget, so constraints get their own field, the longest
/// half-life, and the top render tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    /// Slug of `rule`; the merge key.
    pub id: String,
    pub kind: ConstraintKind,
    /// The rule as a bare predicate, no leading "never": "suggest Kubernetes".
    pub rule: String,
    /// Where it applies ("the superleo repo", "voice replies"); `None` = always.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(flatten)]
    pub prov: Provenance,
}

/// Something the user keeps coming back to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub prov: Provenance,
}

/// A person the user talks about often enough that the agent should know who
/// they are without being reminded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    pub id: String,
    pub name: String,
    /// "co-founder", "manager", "daughter".
    pub relation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(flatten)]
    pub prov: Provenance,
}

/// Something the agent has noticed it does not know and should resolve when a
/// natural opening appears. This is what makes the model *deepen* instead of
/// merely accumulate: it records the shape of its own ignorance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenQuestion {
    pub id: String,
    pub question: String,
    /// Why answering it would help. Keeps the agent from asking noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(flatten)]
    pub prov: Provenance,
}

/// The versioned, per-user portrait.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserModel {
    pub schema_version: u32,
    /// Whose portrait this is. Checked on load against the id that was asked
    /// for; a mismatch is a tenant leak, not a warning.
    pub user_id: UserId,
    /// Monotonic; incremented by every merge. Storage backends are frequently
    /// append-only, so "which of these records is current" has to be answerable
    /// from the record itself.
    pub revision: u64,
    pub updated_ms: i64,
    #[serde(default)]
    pub identity: Identity,
    #[serde(default)]
    pub communication: CommunicationPrefs,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<Constraint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expertise: Vec<DomainExpertise>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goals: Vec<Goal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relationships: Vec<Relationship>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<OpenQuestion>,
}

impl UserModel {
    /// An empty portrait for `user`. Note there is no `Default`: a portrait
    /// without an owner is exactly the bug this crate is trying to prevent.
    pub fn new(user: UserId) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            user_id: user,
            revision: 0,
            updated_ms: 0,
            identity: Identity::default(),
            communication: CommunicationPrefs::default(),
            constraints: Vec::new(),
            expertise: Vec::new(),
            goals: Vec::new(),
            relationships: Vec::new(),
            open_questions: Vec::new(),
        }
    }

    /// True when nothing at all is known — the guide injects nothing in that
    /// case rather than spending tokens on an empty heading.
    pub fn is_empty(&self) -> bool {
        self.identity == Identity::default()
            && self.communication == CommunicationPrefs::default()
            && self.constraints.is_empty()
            && self.expertise.is_empty()
            && self.goals.is_empty()
            && self.relationships.is_empty()
            && self.open_questions.is_empty()
    }
}

// ---------------------------------------------------------------------------
// policy
// ---------------------------------------------------------------------------

/// Per-category half-lives, in days, plus the floor below which a belief is
/// dropped.
///
/// Different kinds of truth rot at different rates, and using one global TTL
/// would force a choice between forgetting a user's name and remembering a
/// finished project forever. The numbers below are deliberately coarse; they
/// only ever decide ordering and an eventual drop.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AgingPolicy {
    /// Names, roles, employers: change on the order of years.
    pub identity_days: f32,
    /// Skill grows; a year-old "novice" is probably wrong.
    pub expertise_days: f32,
    /// Style preferences drift with mood and medium.
    pub communication_days: f32,
    /// Standing rules hold until explicitly revoked. Near-permanent on purpose:
    /// re-suggesting the thing the user banned is the worst failure mode here.
    pub constraint_days: f32,
    /// Projects churn fastest of all.
    pub goal_days: f32,
    pub relationship_days: f32,
    /// A question nobody has answered in a month was not worth asking.
    pub question_days: f32,
    /// Effective confidence below which an item is pruned / not rendered.
    pub prune_below: f32,
}

impl Default for AgingPolicy {
    fn default() -> Self {
        Self {
            identity_days: 365.0,
            expertise_days: 180.0,
            communication_days: 120.0,
            constraint_days: 730.0,
            goal_days: 45.0,
            relationship_days: 365.0,
            question_days: 30.0,
            prune_below: 0.15,
        }
    }
}

/// Everything the merge and the renderer need to know that is policy rather
/// than data. Kept out of [`UserModel`] so a stored portrait never pins an
/// operator to yesterday's tuning.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PortraitPolicy {
    pub aging: AgingPolicy,
    /// Hard ceiling on the rendered block, marker and header included.
    /// ~1200 chars is roughly 300 tokens: affordable on every single turn,
    /// which is the whole point — a portrait nobody can afford to inject is
    /// not a portrait.
    pub render_budget_chars: usize,
    /// Per-item clip length, so one rambling constraint cannot eat the budget.
    pub max_item_chars: usize,
    /// Rendered items per section.
    pub max_items_per_section: usize,
    /// Stored items per collection. Storage is cheap but unbounded growth
    /// makes every later merge and render slower for no benefit.
    pub max_items_stored: usize,
}

impl Default for PortraitPolicy {
    fn default() -> Self {
        Self {
            aging: AgingPolicy::default(),
            render_budget_chars: 1200,
            max_item_chars: 120,
            max_items_per_section: 6,
            max_items_stored: 24,
        }
    }
}

// ---------------------------------------------------------------------------
// delta
// ---------------------------------------------------------------------------

/// One thing the model claims to have noticed. Observations are *proposals*:
/// they say what was seen, never what the portrait should become. All merge
/// decisions — reinforce, overrule, reject, prune — are made by the Rust code
/// in [`UserModel::merge`], so a model that hallucinates cannot rewrite the
/// portrait wholesale, only add evidence that the merge rules then weigh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Observation {
    Identity {
        field: IdentityField,
        value: String,
        confidence: f32,
    },
    Expertise {
        domain: String,
        level: ExpertiseLevel,
        #[serde(default)]
        note: Option<String>,
        confidence: f32,
    },
    Communication {
        field: CommField,
        value: String,
        confidence: f32,
    },
    StyleNote {
        text: String,
        confidence: f32,
    },
    Constraint {
        /// `never` / `always` / `avoid`.
        mode: ConstraintKind,
        rule: String,
        #[serde(default)]
        scope: Option<String>,
        confidence: f32,
    },
    Goal {
        title: String,
        status: GoalStatus,
        #[serde(default)]
        detail: Option<String>,
        confidence: f32,
    },
    Relationship {
        name: String,
        relation: String,
        #[serde(default)]
        note: Option<String>,
        confidence: f32,
    },
    OpenQuestion {
        question: String,
        #[serde(default)]
        why: Option<String>,
        confidence: f32,
    },
}

/// What one update round proposes. Never applied verbatim.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UserModelDelta {
    #[serde(default, deserialize_with = "lenient_observations")]
    pub observations: Vec<Observation>,
    /// Ids (or free text that slugs to an id) of open questions now answered.
    #[serde(default)]
    pub resolved_questions: Vec<String>,
    /// Ids of items the user explicitly revoked ("forget that I …"). Applied as
    /// a hard delete — an explicit retraction outranks any confidence.
    #[serde(default)]
    pub retracted: Vec<String>,
    /// Where this evidence came from; copied into every touched provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Skip observations we cannot parse instead of failing the whole delta.
///
/// Models invent enum variants. Losing one bad observation costs nothing;
/// losing the eight good ones alongside it costs a whole update round, and the
/// round only happens once every N turns.
fn lenient_observations<'de, D>(d: D) -> Result<Vec<Observation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<serde_json::Value> = Vec::deserialize(d)?;
    let mut out = Vec::with_capacity(raw.len());
    for v in raw {
        match serde_json::from_value::<Observation>(v) {
            Ok(o) => out.push(o),
            Err(e) => tracing::debug!(error = %e, "user model: skipped unparsable observation"),
        }
    }
    Ok(out)
}

impl UserModelDelta {
    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
            && self.resolved_questions.is_empty()
            && self.retracted.is_empty()
    }
}

/// What a merge actually did. Returned rather than logged so callers can decide
/// whether the round was worth persisting, and so tests can assert on the
/// resolution rules directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Claims that did not exist before.
    pub added: u32,
    /// Claims restated by new evidence; confidence went up.
    pub reinforced: u32,
    /// Conflicts where the new evidence won and the old value was superseded.
    pub overruled: u32,
    /// Conflicts where the incumbent held; its confidence was eroded.
    pub held: u32,
    /// Explicit retractions + resolved questions.
    pub removed: u32,
    /// Aged out below the confidence floor, or trimmed for size.
    pub pruned: u32,
    /// Observations the rules refused (empty text, unusable value).
    pub ignored: u32,
}

impl MergeReport {
    pub fn changed(&self) -> bool {
        self.added + self.reinforced + self.overruled + self.held + self.removed + self.pruned > 0
    }
}

// ---------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------

enum Outcome {
    Reinforced,
    Overruled,
    Held,
}

/// Restate an existing belief: noisy-OR against the *decayed* confidence, then
/// restart the decay clock. Using the decayed value as the base means a claim
/// that has not been mentioned in a year comes back as "believed again",
/// not "believed as strongly as it ever was".
fn reinforce(
    prov: &mut Provenance,
    confidence: f32,
    now_ms: i64,
    half_life_days: f32,
    source: &Option<String>,
) {
    let conf = clamp_conf(confidence);
    let eff = prov.effective_confidence(now_ms, half_life_days);
    prov.confidence = (eff + conf * (1.0 - eff)).min(0.98);
    prov.updated_ms = now_ms;
    prov.evidence_count = prov.evidence_count.saturating_add(1);
    if source.is_some() {
        prov.source = source.clone();
    }
}

/// The conflict rule, in one place.
///
/// **Recency-dominant with confidence hysteresis.** New evidence replaces the
/// incumbent iff its confidence is at least the incumbent's *time-decayed*
/// confidence; otherwise the incumbent survives with its stored confidence
/// eroded and its `updated_ms` left alone.
///
/// Why this and not "newest always wins": a single 0.3-confidence inference
/// should not overwrite something the user has stated outright three times.
/// Why this and not "highest confidence wins": people change their minds, and a
/// portrait where a stale assertion can outvote what the user said this morning
/// is actively harmful. Comparing against the *decayed* incumbent gives both —
/// freshly-reinforced beliefs are hard to flip, old ones are easy, and no
/// separate rule is needed for either case.
///
/// The loser is never destroyed: it is recorded in `superseded` and the
/// survivor is flagged `contradicted`, which the renderer shows as `(?)`.
/// Erosion without touching `updated_ms` means a claim contradicted repeatedly
/// flips on its own after a few rounds, so a genuinely changed fact does not
/// need a special case either.
fn contest<T: PartialEq + fmt::Display>(
    current: &mut T,
    incoming: T,
    prov: &mut Provenance,
    confidence: f32,
    now_ms: i64,
    half_life_days: f32,
    source: &Option<String>,
) -> Outcome {
    let conf = clamp_conf(confidence);
    let eff = prov.effective_confidence(now_ms, half_life_days);

    if *current == incoming {
        reinforce(prov, confidence, now_ms, half_life_days, source);
        return Outcome::Reinforced;
    }

    if conf >= eff {
        prov.superseded = Some(current.to_string());
        *current = incoming;
        prov.confidence = conf;
        prov.updated_ms = now_ms;
        prov.evidence_count = 1;
        prov.contradicted = true;
        if source.is_some() {
            prov.source = source.clone();
        }
        Outcome::Overruled
    } else {
        prov.superseded = Some(incoming.to_string());
        prov.contradicted = true;
        // Erode the STORED confidence, not the decayed one, and leave
        // `updated_ms` where it was — otherwise the age discount would be
        // double-counted next round and the decay clock would reset on a claim
        // nobody actually reaffirmed.
        prov.confidence = (prov.confidence * (1.0 - 0.5 * conf)).max(0.0);
        Outcome::Held
    }
}

fn record(report: &mut MergeReport, outcome: Outcome) {
    match outcome {
        Outcome::Reinforced => report.reinforced += 1,
        Outcome::Overruled => report.overruled += 1,
        Outcome::Held => report.held += 1,
    }
}

/// Stable, deterministic merge key for free text. CJK survives intact because
/// `char::is_alphanumeric` is Unicode-aware.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in s.trim().to_lowercase().chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.chars().take(64).collect()
}

impl UserModel {
    /// Apply a proposed delta. Deterministic: same portrait + same delta +
    /// same `now_ms` always yields the same portrait, whatever the model said.
    ///
    /// Order of operations matters. Retractions run last so that an
    /// observation and a retraction of the same thing in one delta resolve to
    /// "gone" — the user asking to forget something outranks the model
    /// noticing it. Pruning runs after that so the returned portrait is always
    /// already within policy.
    pub fn merge(
        &mut self,
        delta: &UserModelDelta,
        now_ms: i64,
        policy: &PortraitPolicy,
    ) -> MergeReport {
        let mut report = MergeReport::default();
        let src = &delta.source;
        let a = &policy.aging;

        for obs in &delta.observations {
            match obs {
                Observation::Identity {
                    field,
                    value,
                    confidence,
                } => {
                    let value = value.trim().to_string();
                    if value.is_empty() {
                        report.ignored += 1;
                        continue;
                    }
                    let slot = match field {
                        IdentityField::DisplayName => &mut self.identity.display_name,
                        IdentityField::Role => &mut self.identity.role,
                        IdentityField::Org => &mut self.identity.org,
                        IdentityField::Locale => &mut self.identity.locale,
                        IdentityField::Timezone => &mut self.identity.timezone,
                    };
                    merge_slot(
                        slot,
                        value,
                        *confidence,
                        now_ms,
                        a.identity_days,
                        src,
                        &mut report,
                    );
                }

                Observation::Communication {
                    field,
                    value,
                    confidence,
                } => match field {
                    CommField::Language => {
                        let v = value.trim().to_string();
                        // A language tag is short and token-like; anything else
                        // is the model narrating, and narration in this slot
                        // would be injected into every prompt forever.
                        if v.is_empty() || v.chars().count() > 16 {
                            report.ignored += 1;
                            continue;
                        }
                        merge_slot(
                            &mut self.communication.language,
                            v,
                            *confidence,
                            now_ms,
                            a.communication_days,
                            src,
                            &mut report,
                        );
                    }
                    CommField::Verbosity => match value.parse::<Verbosity>() {
                        Ok(v) => merge_slot(
                            &mut self.communication.verbosity,
                            v,
                            *confidence,
                            now_ms,
                            a.communication_days,
                            src,
                            &mut report,
                        ),
                        Err(()) => report.ignored += 1,
                    },
                    CommField::Formality => match value.parse::<Formality>() {
                        Ok(v) => merge_slot(
                            &mut self.communication.formality,
                            v,
                            *confidence,
                            now_ms,
                            a.communication_days,
                            src,
                            &mut report,
                        ),
                        Err(()) => report.ignored += 1,
                    },
                },

                Observation::StyleNote { text, confidence } => {
                    let id = slug(text);
                    if id.is_empty() {
                        report.ignored += 1;
                        continue;
                    }
                    match self.communication.notes.iter_mut().find(|n| n.id == id) {
                        // Same slug means same note: nothing to contest, only
                        // to reaffirm.
                        Some(n) => {
                            reinforce(&mut n.prov, *confidence, now_ms, a.communication_days, src);
                            report.reinforced += 1;
                        }
                        None => {
                            self.communication.notes.push(StyleNote {
                                id,
                                text: text.trim().to_string(),
                                prov: Provenance::new(now_ms, *confidence, src.clone()),
                            });
                            report.added += 1;
                        }
                    }
                }

                Observation::Expertise {
                    domain,
                    level,
                    note,
                    confidence,
                } => {
                    let id = slug(domain);
                    if id.is_empty() {
                        report.ignored += 1;
                        continue;
                    }
                    match self.expertise.iter_mut().find(|e| e.domain == id) {
                        Some(e) => {
                            let o = contest(
                                &mut e.level,
                                *level,
                                &mut e.prov,
                                *confidence,
                                now_ms,
                                a.expertise_days,
                                src,
                            );
                            if note.is_some() && !matches!(o, Outcome::Held) {
                                e.note = note.clone();
                            }
                            record(&mut report, o);
                        }
                        None => {
                            self.expertise.push(DomainExpertise {
                                domain: id,
                                level: *level,
                                note: note.clone(),
                                prov: Provenance::new(now_ms, *confidence, src.clone()),
                            });
                            report.added += 1;
                        }
                    }
                }

                Observation::Constraint {
                    mode,
                    rule,
                    scope,
                    confidence,
                } => {
                    let id = slug(rule);
                    if id.is_empty() {
                        report.ignored += 1;
                        continue;
                    }
                    match self.constraints.iter_mut().find(|c| c.id == id) {
                        Some(c) => {
                            let o = contest(
                                &mut c.kind,
                                *mode,
                                &mut c.prov,
                                *confidence,
                                now_ms,
                                a.constraint_days,
                                src,
                            );
                            if scope.is_some() && !matches!(o, Outcome::Held) {
                                c.scope = scope.clone();
                            }
                            record(&mut report, o);
                        }
                        None => {
                            self.constraints.push(Constraint {
                                id,
                                kind: *mode,
                                rule: rule.trim().to_string(),
                                scope: scope.clone(),
                                prov: Provenance::new(now_ms, *confidence, src.clone()),
                            });
                            report.added += 1;
                        }
                    }
                }

                Observation::Goal {
                    title,
                    status,
                    detail,
                    confidence,
                } => {
                    let id = slug(title);
                    if id.is_empty() {
                        report.ignored += 1;
                        continue;
                    }
                    match self.goals.iter_mut().find(|g| g.id == id) {
                        Some(g) => {
                            let o = contest(
                                &mut g.status,
                                *status,
                                &mut g.prov,
                                *confidence,
                                now_ms,
                                a.goal_days,
                                src,
                            );
                            if detail.is_some() && !matches!(o, Outcome::Held) {
                                g.detail = detail.clone();
                            }
                            record(&mut report, o);
                        }
                        None => {
                            self.goals.push(Goal {
                                id,
                                title: title.trim().to_string(),
                                status: *status,
                                detail: detail.clone(),
                                prov: Provenance::new(now_ms, *confidence, src.clone()),
                            });
                            report.added += 1;
                        }
                    }
                }

                Observation::Relationship {
                    name,
                    relation,
                    note,
                    confidence,
                } => {
                    let id = slug(name);
                    if id.is_empty() {
                        report.ignored += 1;
                        continue;
                    }
                    match self.relationships.iter_mut().find(|r| r.id == id) {
                        Some(r) => {
                            let o = contest(
                                &mut r.relation,
                                relation.trim().to_string(),
                                &mut r.prov,
                                *confidence,
                                now_ms,
                                a.relationship_days,
                                src,
                            );
                            if note.is_some() && !matches!(o, Outcome::Held) {
                                r.note = note.clone();
                            }
                            record(&mut report, o);
                        }
                        None => {
                            self.relationships.push(Relationship {
                                id,
                                name: name.trim().to_string(),
                                relation: relation.trim().to_string(),
                                note: note.clone(),
                                prov: Provenance::new(now_ms, *confidence, src.clone()),
                            });
                            report.added += 1;
                        }
                    }
                }

                Observation::OpenQuestion {
                    question,
                    why,
                    confidence,
                } => {
                    let id = slug(question);
                    if id.is_empty() {
                        report.ignored += 1;
                        continue;
                    }
                    match self.open_questions.iter_mut().find(|q| q.id == id) {
                        // Re-noticing an unanswered question keeps it alive
                        // against its (short) half-life. Nothing to contest.
                        Some(q) => {
                            reinforce(&mut q.prov, *confidence, now_ms, a.question_days, src);
                            report.reinforced += 1;
                        }
                        None => {
                            self.open_questions.push(OpenQuestion {
                                id,
                                question: question.trim().to_string(),
                                why: why.clone(),
                                prov: Provenance::new(now_ms, *confidence, src.clone()),
                            });
                            report.added += 1;
                        }
                    }
                }
            }
        }

        for q in &delta.resolved_questions {
            let id = slug(q);
            let before = self.open_questions.len();
            self.open_questions.retain(|x| x.id != id);
            report.removed += (before - self.open_questions.len()) as u32;
        }

        for r in &delta.retracted {
            report.removed += self.retract(&slug(r));
        }

        report.pruned = self.prune(now_ms, policy);
        self.revision = self.revision.saturating_add(1);
        self.updated_ms = now_ms;
        report
    }

    /// Hard-delete anything keyed by `id`, across every collection and slot.
    /// An explicit "forget that" is the one input that bypasses confidence
    /// entirely.
    fn retract(&mut self, id: &str) -> u32 {
        let mut n = 0u32;
        let mut drop_slot = |present: bool, slot_id: &str| -> bool {
            if present && slot_id == id {
                n += 1;
                true
            } else {
                false
            }
        };
        if drop_slot(self.identity.display_name.is_some(), "display-name") {
            self.identity.display_name = None;
        }
        if drop_slot(self.identity.role.is_some(), "role") {
            self.identity.role = None;
        }
        if drop_slot(self.identity.org.is_some(), "org") {
            self.identity.org = None;
        }
        if drop_slot(self.identity.locale.is_some(), "locale") {
            self.identity.locale = None;
        }
        if drop_slot(self.identity.timezone.is_some(), "timezone") {
            self.identity.timezone = None;
        }
        if drop_slot(self.communication.language.is_some(), "language") {
            self.communication.language = None;
        }
        if drop_slot(self.communication.verbosity.is_some(), "verbosity") {
            self.communication.verbosity = None;
        }
        if drop_slot(self.communication.formality.is_some(), "formality") {
            self.communication.formality = None;
        }

        macro_rules! drop_from {
            ($v:expr, $field:ident) => {{
                let before = $v.len();
                $v.retain(|x| x.$field != id);
                n += (before - $v.len()) as u32;
            }};
        }
        drop_from!(self.communication.notes, id);
        drop_from!(self.constraints, id);
        drop_from!(self.expertise, domain);
        drop_from!(self.goals, id);
        drop_from!(self.relationships, id);
        drop_from!(self.open_questions, id);
        n
    }

    /// Drop beliefs whose effective confidence has decayed below the floor, and
    /// trim any collection over `max_items_stored` (keeping the strongest).
    /// Returns how many items went away.
    ///
    /// Called automatically at the end of every [`UserModel::merge`]; exposed
    /// separately for a periodic janitor on portraits that are read far more
    /// often than they are updated.
    pub fn prune(&mut self, now_ms: i64, policy: &PortraitPolicy) -> u32 {
        let floor = policy.aging.prune_below;
        let a = &policy.aging;
        let mut n = 0u32;

        macro_rules! prune_slot {
            ($slot:expr, $hl:expr) => {
                if $slot
                    .as_ref()
                    .is_some_and(|s| s.prov.effective_confidence(now_ms, $hl) < floor)
                {
                    $slot = None;
                    n += 1;
                }
            };
        }
        prune_slot!(self.identity.display_name, a.identity_days);
        prune_slot!(self.identity.role, a.identity_days);
        prune_slot!(self.identity.org, a.identity_days);
        prune_slot!(self.identity.locale, a.identity_days);
        prune_slot!(self.identity.timezone, a.identity_days);
        prune_slot!(self.communication.language, a.communication_days);
        prune_slot!(self.communication.verbosity, a.communication_days);
        prune_slot!(self.communication.formality, a.communication_days);

        macro_rules! prune_vec {
            ($v:expr, $hl:expr) => {{
                let before = $v.len();
                $v.retain(|x| x.prov.effective_confidence(now_ms, $hl) >= floor);
                n += (before - $v.len()) as u32;
                if $v.len() > policy.max_items_stored {
                    $v.sort_by(|x, y| {
                        y.prov
                            .effective_confidence(now_ms, $hl)
                            .total_cmp(&x.prov.effective_confidence(now_ms, $hl))
                    });
                    n += ($v.len() - policy.max_items_stored) as u32;
                    $v.truncate(policy.max_items_stored);
                }
            }};
        }
        prune_vec!(self.communication.notes, a.communication_days);
        prune_vec!(self.constraints, a.constraint_days);
        prune_vec!(self.expertise, a.expertise_days);
        prune_vec!(self.goals, a.goal_days);
        prune_vec!(self.relationships, a.relationship_days);
        prune_vec!(self.open_questions, a.question_days);
        n
    }
}

fn merge_slot<T: PartialEq + fmt::Display>(
    slot: &mut Option<Attested<T>>,
    value: T,
    confidence: f32,
    now_ms: i64,
    half_life_days: f32,
    source: &Option<String>,
    report: &mut MergeReport,
) {
    match slot {
        Some(a) => {
            let o = contest(
                &mut a.value,
                value,
                &mut a.prov,
                confidence,
                now_ms,
                half_life_days,
                source,
            );
            record(report, o);
        }
        None => {
            *slot = Some(Attested::new(value, now_ms, confidence, source.clone()));
            report.added += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

/// Render sections, in the order they are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Who,
    Style,
    Rules,
    Expertise,
    Working,
    People,
    Unknowns,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Who => "who",
            Section::Style => "style",
            Section::Rules => "rules",
            Section::Expertise => "expertise",
            Section::Working => "working on",
            Section::People => "people",
            Section::Unknowns => "ask when natural",
        }
    }

    /// Eviction tier. Lower survives longer under budget pressure.
    ///
    /// Tiers exist because "drop the lowest confidence first" alone would let a
    /// well-evidenced hobby fact push out a shakier "never suggest X". The cost
    /// of forgetting is not uniform: violating a standing rule or answering in
    /// the wrong language is a visible failure, while forgetting a colleague's
    /// name is a mild one. Within a tier, and only within a tier, eviction is
    /// by effective confidence — which already folds in age.
    fn tier(self) -> u8 {
        match self {
            Section::Rules | Section::Style => 0,
            Section::Who => 1,
            Section::Expertise | Section::Working => 2,
            Section::People | Section::Unknowns => 3,
        }
    }
}

struct RenderItem {
    section: Section,
    text: String,
    score: f32,
    updated_ms: i64,
}

fn clip(s: &str, max: usize) -> String {
    let t = s.trim().replace('\n', " ");
    if t.chars().count() <= max {
        t
    } else {
        let mut out: String = t.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// `(?)` marks a belief that is either weak or disputed, so the agent hedges
/// instead of asserting. Two characters is the entire mechanism; anything more
/// verbose would not survive the budget.
fn hedge(prov: &Provenance, eff: f32) -> &'static str {
    if prov.contradicted || eff < 0.5 {
        " (?)"
    } else {
        ""
    }
}

impl UserModel {
    /// Render the portrait for prompt injection, within
    /// `policy.render_budget_chars`.
    pub fn render(&self, now_ms: i64, policy: &PortraitPolicy) -> Option<String> {
        self.render_within(now_ms, policy, policy.render_budget_chars)
    }

    /// Render within an explicit char budget (the guide subtracts its marker
    /// from the policy budget before calling this, so the *injected block* is
    /// what the budget actually covers).
    ///
    /// Selection is a single greedy pass over every candidate line sorted by
    /// `(tier, effective confidence desc, recency desc)`. One ordering drives
    /// both which items survive the budget and which get aged out entirely, so
    /// the prompt never disagrees with the store about what matters.
    pub fn render_within(
        &self,
        now_ms: i64,
        policy: &PortraitPolicy,
        budget_chars: usize,
    ) -> Option<String> {
        let a = &policy.aging;
        let floor = a.prune_below;
        let clip_at = policy.max_item_chars;
        let mut items: Vec<RenderItem> = Vec::new();

        let mut push = |section: Section, text: String, prov: &Provenance, hl: f32| {
            let eff = prov.effective_confidence(now_ms, hl);
            if eff < floor || text.is_empty() {
                return;
            }
            items.push(RenderItem {
                section,
                text,
                score: eff,
                updated_ms: prov.updated_ms,
            });
        };

        // who
        for (label, slot) in [
            ("name", &self.identity.display_name),
            ("role", &self.identity.role),
            ("org", &self.identity.org),
            ("locale", &self.identity.locale),
            ("tz", &self.identity.timezone),
        ] {
            if let Some(v) = slot {
                let eff = v.prov.effective_confidence(now_ms, a.identity_days);
                push(
                    Section::Who,
                    format!("{label}={}{}", clip(&v.value, clip_at), hedge(&v.prov, eff)),
                    &v.prov,
                    a.identity_days,
                );
            }
        }

        // style
        if let Some(v) = &self.communication.language {
            let eff = v.prov.effective_confidence(now_ms, a.communication_days);
            push(
                Section::Style,
                format!(
                    "reply in {}{}",
                    clip(&v.value, clip_at),
                    hedge(&v.prov, eff)
                ),
                &v.prov,
                a.communication_days,
            );
        }
        if let Some(v) = &self.communication.verbosity {
            let eff = v.prov.effective_confidence(now_ms, a.communication_days);
            push(
                Section::Style,
                format!("{}{}", v.value, hedge(&v.prov, eff)),
                &v.prov,
                a.communication_days,
            );
        }
        if let Some(v) = &self.communication.formality {
            let eff = v.prov.effective_confidence(now_ms, a.communication_days);
            push(
                Section::Style,
                format!("{}{}", v.value, hedge(&v.prov, eff)),
                &v.prov,
                a.communication_days,
            );
        }
        for n in &self.communication.notes {
            let eff = n.prov.effective_confidence(now_ms, a.communication_days);
            push(
                Section::Style,
                format!("{}{}", clip(&n.text, clip_at), hedge(&n.prov, eff)),
                &n.prov,
                a.communication_days,
            );
        }

        // rules
        for c in &self.constraints {
            let eff = c.prov.effective_confidence(now_ms, a.constraint_days);
            let scope = c
                .scope
                .as_deref()
                .map(|s| format!(" [{}]", clip(s, 32)))
                .unwrap_or_default();
            push(
                Section::Rules,
                format!(
                    "{} {}{scope}{}",
                    c.kind,
                    clip(&c.rule, clip_at),
                    hedge(&c.prov, eff)
                ),
                &c.prov,
                a.constraint_days,
            );
        }

        // expertise
        for e in &self.expertise {
            let eff = e.prov.effective_confidence(now_ms, a.expertise_days);
            push(
                Section::Expertise,
                format!("{}={}{}", clip(&e.domain, 40), e.level, hedge(&e.prov, eff)),
                &e.prov,
                a.expertise_days,
            );
        }

        // working on — finished work is history, not context
        for g in &self.goals {
            if matches!(g.status, GoalStatus::Done | GoalStatus::Abandoned) {
                continue;
            }
            let eff = g.prov.effective_confidence(now_ms, a.goal_days);
            let status = if g.status == GoalStatus::Active {
                String::new()
            } else {
                format!(" ({})", g.status)
            };
            push(
                Section::Working,
                format!("{}{status}{}", clip(&g.title, clip_at), hedge(&g.prov, eff)),
                &g.prov,
                a.goal_days,
            );
        }

        // people
        for r in &self.relationships {
            let eff = r.prov.effective_confidence(now_ms, a.relationship_days);
            push(
                Section::People,
                format!(
                    "{} = {}{}",
                    clip(&r.name, 40),
                    clip(&r.relation, 40),
                    hedge(&r.prov, eff)
                ),
                &r.prov,
                a.relationship_days,
            );
        }

        // unknowns
        for q in &self.open_questions {
            push(
                Section::Unknowns,
                clip(&q.question, clip_at),
                &q.prov,
                a.question_days,
            );
        }

        if items.is_empty() {
            return None;
        }

        // Stable sort: tier, then strength, then recency. Ties fall back to
        // insertion order so the rendering is byte-identical run to run.
        items.sort_by(|x, y| {
            x.section
                .tier()
                .cmp(&y.section.tier())
                .then(y.score.total_cmp(&x.score))
                .then(y.updated_ms.cmp(&x.updated_ms))
        });

        let header = "What is known about this user, strongest first. Follow it; do not recite it. `(?)` = unverified.";
        let mut used = header.chars().count() + 1; // + newline
        let mut chosen: Vec<&RenderItem> = Vec::new();
        let mut per_section: std::collections::BTreeMap<Section, usize> = Default::default();

        for it in &items {
            let n = per_section.entry(it.section).or_insert(0);
            if *n >= policy.max_items_per_section {
                continue;
            }
            // "; " between items, and each new section costs its own line and
            // label. Charge both, so the budget is a real ceiling rather than
            // an estimate that overshoots on the last item.
            let cost = it.text.chars().count()
                + if *n == 0 {
                    it.section.label().chars().count() + 3 // "\nlabel: "
                } else {
                    2 // "; "
                };
            if used + cost > budget_chars {
                continue;
            }
            used += cost;
            *n += 1;
            chosen.push(it);
        }

        if chosen.is_empty() {
            return None;
        }

        let mut out = String::from(header);
        for section in [
            Section::Who,
            Section::Style,
            Section::Rules,
            Section::Expertise,
            Section::Working,
            Section::People,
            Section::Unknowns,
        ] {
            let line: Vec<&str> = chosen
                .iter()
                .filter(|i| i.section == section)
                .map(|i| i.text.as_str())
                .collect();
            if line.is_empty() {
                continue;
            }
            out.push('\n');
            out.push_str(section.label());
            out.push_str(": ");
            out.push_str(&line.join("; "));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn pol() -> PortraitPolicy {
        PortraitPolicy::default()
    }

    fn constraint(rule: &str, mode: ConstraintKind, c: f32) -> Observation {
        Observation::Constraint {
            mode,
            rule: rule.into(),
            scope: None,
            confidence: c,
        }
    }

    #[test]
    fn new_facts_are_added_with_provenance() {
        let mut m = UserModel::new(UserId::new("u1"));
        let delta = UserModelDelta {
            observations: vec![
                Observation::Identity {
                    field: IdentityField::Role,
                    value: "solo founder".into(),
                    confidence: 0.9,
                },
                Observation::Expertise {
                    domain: "Rust".into(),
                    level: ExpertiseLevel::Expert,
                    note: None,
                    confidence: 0.8,
                },
                constraint("suggest Kubernetes", ConstraintKind::Never, 0.9),
            ],
            source: Some("sess-1".into()),
            ..Default::default()
        };
        let r = m.merge(&delta, DAY, &pol());

        assert_eq!(r.added, 3);
        assert_eq!(m.revision, 1);
        assert_eq!(m.identity.role.as_ref().unwrap().value, "solo founder");
        // domain is slugged so "Rust" and "rust" are the same belief
        assert_eq!(m.expertise[0].domain, "rust");
        let prov = &m.constraints[0].prov;
        assert_eq!(prov.learned_ms, DAY);
        assert_eq!(prov.evidence_count, 1);
        assert_eq!(prov.source.as_deref(), Some("sess-1"));
        assert!(!prov.contradicted);
    }

    #[test]
    fn restating_a_fact_reinforces_it() {
        let mut m = UserModel::new(UserId::new("u1"));
        let d = UserModelDelta {
            observations: vec![Observation::Communication {
                field: CommField::Verbosity,
                value: "terse".into(),
                confidence: 0.6,
            }],
            ..Default::default()
        };
        m.merge(&d, DAY, &pol());
        let first = m.communication.verbosity.as_ref().unwrap().prov.confidence;
        let r = m.merge(&d, DAY * 2, &pol());

        assert_eq!(r.reinforced, 1);
        let p = &m.communication.verbosity.as_ref().unwrap().prov;
        assert!(p.confidence > first, "{} !> {first}", p.confidence);
        assert_eq!(p.evidence_count, 2);
        assert_eq!(p.learned_ms, DAY, "learned_ms is the audit trail; it stays");
        assert_eq!(p.updated_ms, DAY * 2);
    }

    #[test]
    fn fresh_strong_evidence_overrules_a_decayed_incumbent() {
        // Rule: incoming wins iff confidence >= the incumbent's DECAYED
        // confidence. A year-old 0.9 belief has decayed well under a fresh 0.6.
        let mut m = UserModel::new(UserId::new("u1"));
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Expertise {
                    domain: "rust".into(),
                    level: ExpertiseLevel::Novice,
                    note: None,
                    confidence: 0.9,
                }],
                ..Default::default()
            },
            0,
            &pol(),
        );

        let later = DAY * 400;
        let r = m.merge(
            &UserModelDelta {
                observations: vec![Observation::Expertise {
                    domain: "rust".into(),
                    level: ExpertiseLevel::Expert,
                    note: None,
                    confidence: 0.6,
                }],
                ..Default::default()
            },
            later,
            &pol(),
        );

        assert_eq!(r.overruled, 1);
        let e = &m.expertise[0];
        assert_eq!(e.level, ExpertiseLevel::Expert);
        assert!(e.prov.contradicted);
        assert_eq!(
            e.prov.superseded.as_deref(),
            Some("novice"),
            "the loser is recorded, never destroyed"
        );
        assert_eq!(e.prov.learned_ms, 0);
    }

    #[test]
    fn weak_evidence_does_not_flip_a_fresh_incumbent() {
        let mut m = UserModel::new(UserId::new("u1"));
        m.merge(
            &UserModelDelta {
                observations: vec![constraint("suggest Kubernetes", ConstraintKind::Never, 0.9)],
                ..Default::default()
            },
            DAY,
            &pol(),
        );
        let before = m.constraints[0].prov.confidence;

        let r = m.merge(
            &UserModelDelta {
                observations: vec![constraint("suggest Kubernetes", ConstraintKind::Avoid, 0.2)],
                ..Default::default()
            },
            DAY * 2,
            &pol(),
        );

        assert_eq!(r.held, 1);
        let c = &m.constraints[0];
        assert_eq!(c.kind, ConstraintKind::Never, "incumbent holds");
        assert!(c.prov.contradicted, "but the dispute is recorded");
        assert_eq!(c.prov.superseded.as_deref(), Some("avoid"));
        assert!(c.prov.confidence < before, "and its confidence is eroded");
        assert_eq!(
            c.prov.updated_ms, DAY,
            "the decay clock must not restart on a claim nobody reaffirmed"
        );
    }

    #[test]
    fn repeated_contradiction_eventually_flips_the_incumbent() {
        // Erosion-without-clock-reset is what lets a genuinely changed fact win
        // without any special case for it.
        let mut m = UserModel::new(UserId::new("u1"));
        m.merge(
            &UserModelDelta {
                observations: vec![constraint("use tabs", ConstraintKind::Always, 0.9)],
                ..Default::default()
            },
            DAY,
            &pol(),
        );
        let challenge = UserModelDelta {
            observations: vec![constraint("use tabs", ConstraintKind::Never, 0.5)],
            ..Default::default()
        };
        let mut flipped_at = None;
        for round in 1..=6 {
            let r = m.merge(&challenge, DAY * (1 + round), &pol());
            if r.overruled == 1 {
                flipped_at = Some(round);
                break;
            }
        }
        assert!(
            flipped_at.is_some(),
            "never flipped: {:?}",
            m.constraints[0]
        );
        assert_eq!(m.constraints[0].kind, ConstraintKind::Never);
    }

    #[test]
    fn stale_beliefs_age_out_but_standing_rules_survive() {
        let mut m = UserModel::new(UserId::new("u1"));
        m.merge(
            &UserModelDelta {
                observations: vec![
                    Observation::Goal {
                        title: "ship the beta".into(),
                        status: GoalStatus::Active,
                        detail: None,
                        confidence: 0.8,
                    },
                    constraint("suggest Kubernetes", ConstraintKind::Never, 0.8),
                ],
                ..Default::default()
            },
            0,
            &pol(),
        );
        assert_eq!(m.goals.len(), 1);

        // A year on: goals (45d half-life) are long gone, the standing rule
        // (730d) is still comfortably above the floor.
        let pruned = m.prune(DAY * 365, &pol());
        assert_eq!(pruned, 1);
        assert!(m.goals.is_empty(), "stale goal aged out");
        assert_eq!(m.constraints.len(), 1, "standing rule survives");
    }

    #[test]
    fn retraction_beats_any_confidence() {
        let mut m = UserModel::new(UserId::new("u1"));
        m.merge(
            &UserModelDelta {
                observations: vec![constraint("use tabs", ConstraintKind::Always, 0.95)],
                ..Default::default()
            },
            DAY,
            &pol(),
        );
        let r = m.merge(
            &UserModelDelta {
                retracted: vec!["use tabs".into()],
                ..Default::default()
            },
            DAY * 2,
            &pol(),
        );
        assert_eq!(r.removed, 1);
        assert!(m.constraints.is_empty());
    }

    #[test]
    fn unparsable_observations_are_ignored_not_fatal() {
        let json = r#"{"observations":[
            {"kind":"communication","field":"verbosity","value":"screaming","confidence":0.9},
            {"kind":"telepathy","value":"nope"},
            {"kind":"communication","field":"verbosity","value":"terse","confidence":0.7}
        ]}"#;
        let delta: UserModelDelta = serde_json::from_str(json).unwrap();
        // "telepathy" is dropped at deserialize time, "screaming" at merge time.
        assert_eq!(delta.observations.len(), 2);
        let mut m = UserModel::new(UserId::new("u1"));
        let r = m.merge(&delta, DAY, &pol());
        assert_eq!(r.ignored, 1);
        assert_eq!(
            m.communication.verbosity.as_ref().unwrap().value,
            Verbosity::Terse
        );
    }

    #[test]
    fn render_respects_the_char_budget_and_drops_by_tier_then_confidence() {
        let mut m = UserModel::new(UserId::new("u1"));
        let mut obs = vec![
            constraint(
                "suggest switching to Kubernetes",
                ConstraintKind::Never,
                0.9,
            ),
            Observation::Communication {
                field: CommField::Language,
                value: "zh-CN".into(),
                confidence: 0.9,
            },
        ];
        // Twelve low-tier facts of descending confidence, all far more text
        // than the budget can hold.
        for i in 0..12 {
            obs.push(Observation::Relationship {
                name: format!("colleague number {i}"),
                relation: format!("worked with them on project {i} for a while"),
                note: None,
                confidence: 0.9 - (i as f32) * 0.05,
            });
        }
        m.merge(
            &UserModelDelta {
                observations: obs,
                ..Default::default()
            },
            DAY,
            &pol(),
        );

        let policy = PortraitPolicy {
            render_budget_chars: 320,
            ..pol()
        };
        let out = m.render(DAY, &policy).unwrap();

        assert!(
            out.chars().count() <= 320,
            "budget blown: {} chars\n{out}",
            out.chars().count()
        );
        // Tier 0 always survives.
        assert!(
            out.contains("never suggest switching to Kubernetes"),
            "{out}"
        );
        assert!(out.contains("reply in zh-CN"), "{out}");
        // Within the dropped tier, the strongest survives and the weakest goes.
        assert!(out.contains("colleague number 0"), "{out}");
        assert!(!out.contains("colleague number 11"), "{out}");
    }

    #[test]
    fn render_hedges_weak_and_disputed_beliefs() {
        let mut m = UserModel::new(UserId::new("u1"));
        m.merge(
            &UserModelDelta {
                observations: vec![Observation::Identity {
                    field: IdentityField::Org,
                    value: "Acme".into(),
                    confidence: 0.3,
                }],
                ..Default::default()
            },
            DAY,
            &pol(),
        );
        let out = m.render(DAY, &pol()).unwrap();
        assert!(out.contains("org=Acme (?)"), "{out}");
    }

    #[test]
    fn empty_portrait_renders_nothing() {
        let m = UserModel::new(UserId::new("u1"));
        assert!(m.is_empty());
        assert!(m.render(DAY, &pol()).is_none());
    }

    #[test]
    fn a_full_portrait_costs_about_ninety_tokens() {
        // Golden output. It is here to make the *cost* visible in review: this
        // is a portrait with something in every single section, and it is
        // ~350 chars. If a change pushes this materially up, the "inject it on
        // every turn" premise stops holding.
        let mut m = UserModel::new(UserId::new("li"));
        m.merge(
            &UserModelDelta {
                observations: vec![
                    Observation::Identity {
                        field: IdentityField::DisplayName,
                        value: "李亮".into(),
                        confidence: 0.9,
                    },
                    Observation::Identity {
                        field: IdentityField::Role,
                        value: "solo founder".into(),
                        confidence: 0.8,
                    },
                    Observation::Communication {
                        field: CommField::Language,
                        value: "zh-CN".into(),
                        confidence: 0.9,
                    },
                    Observation::Communication {
                        field: CommField::Verbosity,
                        value: "terse".into(),
                        confidence: 0.8,
                    },
                    Observation::StyleNote {
                        text: "no emoji".into(),
                        confidence: 0.9,
                    },
                    constraint("suggest port 8080", ConstraintKind::Never, 0.9),
                    Observation::Expertise {
                        domain: "rust".into(),
                        level: ExpertiseLevel::Expert,
                        note: None,
                        confidence: 0.8,
                    },
                    Observation::Goal {
                        title: "ship the multi-tenant server".into(),
                        status: GoalStatus::Active,
                        detail: None,
                        confidence: 0.8,
                    },
                    Observation::Relationship {
                        name: "Wei".into(),
                        relation: "co-founder".into(),
                        note: None,
                        confidence: 0.6,
                    },
                    Observation::OpenQuestion {
                        question: "which timezone should scheduling assume?".into(),
                        why: None,
                        confidence: 0.5,
                    },
                ],
                ..Default::default()
            },
            DAY,
            &pol(),
        );

        let out = m.render(DAY, &pol()).unwrap();
        assert_eq!(
            out,
            "What is known about this user, strongest first. Follow it; do not recite it. `(?)` = unverified.\n\
             who: name=李亮; role=solo founder\n\
             style: reply in zh-CN; no emoji; terse\n\
             rules: never suggest port 8080\n\
             expertise: rust=expert\n\
             working on: ship the multi-tenant server\n\
             people: Wei = co-founder\n\
             ask when natural: which timezone should scheduling assume?"
        );
        assert!(out.chars().count() < 400, "{} chars", out.chars().count());
    }

    #[test]
    fn serde_round_trips_including_provenance() {
        let mut m = UserModel::new(UserId::new("用户-1"));
        m.merge(
            &UserModelDelta {
                observations: vec![
                    constraint("suggest Kubernetes", ConstraintKind::Never, 0.9),
                    Observation::OpenQuestion {
                        question: "which timezone should scheduling assume?".into(),
                        why: Some("meeting invites".into()),
                        confidence: 0.5,
                    },
                ],
                ..Default::default()
            },
            DAY,
            &pol(),
        );
        let json = serde_json::to_string(&m).unwrap();
        let back: UserModel = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
