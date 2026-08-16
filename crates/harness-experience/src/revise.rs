//! `SkillReviser` — fix a skill that turned out to be wrong.
//!
//! The other half of Hermes-style continuous learning: a skill is not
//! write-once. When a run followed skill X and went badly, the skill's steps are
//! a prime suspect, and the cheapest moment to fix them is right then, with the
//! failing run still in hand.
//!
//! ```ignore
//! // The run followed `deploy-runbook` and failed.
//! let req = RevisionRequest::from_dir(&skills_root, "deploy-runbook", episode, complaint)?;
//! match reviser.revise(&req).await? {
//!     Revision::Revised(r) => queue_for_review(user_id, *r),   // caller decides
//!     Revision::Refused(w)  => tracing::info!(?w, "revision declined"),
//! }
//! ```
//!
//! ## Drift, and the cap that stops it
//!
//! Every revision is a model rewriting instructions based on one bad run. Ten
//! such rewrites in an afternoon, each triggered by a different flaky failure,
//! will grind a good skill into mush — the classic failure mode of unsupervised
//! self-modification. Two caps, both recorded in the skill's own frontmatter
//! under `metadata.experience` ([`RevisionLedger`]):
//!
//! - **`max_revisions_per_day` (default 2)** — the drift brake. A model looping
//!   on the same flaky failure gets two attempts, then stops until tomorrow. A
//!   day is the right unit because a genuinely wrong skill fails repeatedly
//!   *within a session*, which is exactly the burst this suppresses; a skill
//!   that is wrong for a lasting reason will still be failing tomorrow, and
//!   gets another two attempts then.
//! - **`max_total_revisions` (default 10)** — the lifetime ceiling. A skill that
//!   has been rewritten ten times isn't converging; it's the wrong abstraction,
//!   and the correct action is for a human to retire it, not for the eleventh
//!   rewrite to try again.
//!
//! **Why the ledger lives in the skill file and not in a sidecar table.** The
//! skill directory is the unit that gets copied, synced, and scoped per tenant.
//! An external counter desyncs the instant a skill is moved between users or
//! restored from backup, and a multi-tenant server would need a table whose only
//! job is this. Frontmatter travels with the artefact and — the part that
//! actually matters — is legible to the human reviewing the skill: `revision: 7`
//! on a file is a visible warning that this thing keeps needing fixing. The
//! accepted cost is that a model holding `skill_manage` could edit its own
//! counter. That is not the threat being defended against: the same model could
//! rewrite the entire skill directly. The cap exists to stop an *automated loop*
//! from drifting, not an adversary.
//!
//! ## What a revision may and may not change
//!
//! - **Body**: freely.
//! - **Description**: only when the model returns a `description_change` with a
//!   non-empty justification. Otherwise it is preserved byte for byte.
//! - **Name**: never. Renaming is a delete-plus-create, not a revision: the
//!   directory name, every `skill_manage` reference, every `skill:<name>` tag on
//!   past episodes, and any recall keyed on the old name would silently dangle.
//!   A revision that concludes the skill is misnamed is a signal to retire it,
//!   which is a human's call.

use crate::episode::Episode;
use crate::llm;
use crate::skillmd;
use harness_core::{Model, SkillError, SkillManifest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Frontmatter key holding the revision ledger: `metadata.experience`.
///
/// Deliberately a sibling of `metadata.harness` rather than a child of it.
/// `metadata.harness` deserializes into `harness_core::HarnessExt` (kind, risk,
/// entrypoint); burying unrelated bookkeeping inside a typed struct invites the
/// next person to add `deny_unknown_fields` and silently break this.
pub const EXPERIENCE_METADATA_KEY: &str = "experience";

/// The per-skill revision ledger stored at `metadata.experience`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionLedger {
    /// How many times this skill has been revised. 0 = never.
    #[serde(default)]
    pub revision: u32,
    /// UTC `YYYY-MM-DD` of the most recent revision. Empty = never revised.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub revised_on: String,
    /// Revisions made on [`revised_on`](Self::revised_on). Meaningless once the
    /// date rolls over, which is precisely how the daily cap resets.
    #[serde(default)]
    pub revisions_today: u32,
    /// How this skill came to exist. `Some("distilled")` = machine-authored by
    /// [`crate::SkillDistiller`]; `None` = hand-written or from elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl RevisionLedger {
    /// The ledger a freshly distilled skill starts with.
    pub fn distilled() -> Self {
        Self {
            origin: Some("distilled".into()),
            ..Default::default()
        }
    }

    /// Read the ledger out of a manifest. A skill with no ledger (hand-written,
    /// or predating this feature) reads as all-zero, i.e. never revised — the
    /// permissive default, so an existing skill isn't locked out of its first fix.
    pub fn from_manifest(m: &SkillManifest) -> Self {
        m.metadata
            .get(EXPERIENCE_METADATA_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    /// Write the ledger back into a manifest's metadata, leaving every other
    /// metadata key (notably `metadata.harness`) untouched.
    pub fn write_into(&self, m: &mut SkillManifest) {
        if let Ok(v) = serde_json::to_value(self) {
            m.metadata.insert(EXPERIENCE_METADATA_KEY.to_string(), v);
        }
    }

    /// The ledger after one more revision performed on `today`.
    pub fn bumped(&self, today: &str) -> Self {
        Self {
            revision: self.revision.saturating_add(1),
            revisions_today: if self.revised_on == today {
                self.revisions_today.saturating_add(1)
            } else {
                1
            },
            revised_on: today.to_string(),
            origin: self.origin.clone(),
        }
    }

    /// Revisions already made today, given today's date. Zero once the date
    /// rolls over.
    pub fn revisions_on(&self, today: &str) -> u32 {
        if self.revised_on == today {
            self.revisions_today
        } else {
            0
        }
    }
}

/// When a skill may be revised.
#[derive(Debug, Clone, PartialEq)]
pub struct RevisePolicy {
    /// Lifetime ceiling on revisions. See the module docs.
    pub max_total_revisions: u32,
    /// Ceiling on revisions per UTC day. See the module docs.
    pub max_revisions_per_day: u32,
    /// Refuse unless the skill appears in [`Episode::skills`] — i.e. the run
    /// actually followed it. Turn off only if the host has no way to record
    /// skill usage and is willing to revise on suspicion.
    pub require_skill_in_episode: bool,
    /// Refuse when the episode succeeded. A skill that was followed on a run
    /// that worked has no evidence against it.
    pub refuse_when_episode_succeeded: bool,
    /// Output-token ceiling for the single revision call. Larger than the
    /// distiller's: a revision must reproduce the whole body.
    pub max_output_tokens: u32,
}

impl Default for RevisePolicy {
    fn default() -> Self {
        Self {
            max_total_revisions: 10,
            max_revisions_per_day: 2,
            require_skill_in_episode: true,
            refuse_when_episode_succeeded: true,
            max_output_tokens: 3_000,
        }
    }
}

/// Why a revision was declined. Refusals are normal control flow, not errors.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RefusalReason {
    /// Lifetime cap hit — the skill is the wrong abstraction; retire it.
    LifetimeCapReached { revision: u32, max: u32 },
    /// Daily cap hit — try again tomorrow.
    DailyCapReached { today: u32, max: u32, date: String },
    /// The run never followed this skill, so it isn't evidence about it.
    SkillNotUsedInEpisode { skill: String },
    /// The run worked. Nothing to fix.
    EpisodeSucceeded,
}

/// Outcome of [`SkillReviser::check`] — the cap decision, with no model call.
#[derive(Debug, Clone, PartialEq)]
pub enum RevisionGate {
    Revise {
        /// Revisions this skill has already had, lifetime.
        current_revision: u32,
        /// …and today.
        revisions_today: u32,
    },
    Refuse(RefusalReason),
}

/// Outcome of [`SkillReviser::revise`].
#[derive(Debug, Clone, PartialEq)]
pub enum Revision {
    /// Boxed because `SkillRevision` carries the full skill body, and an enum is
    /// as large as its largest variant.
    Revised(Box<SkillRevision>),
    Refused(RefusalReason),
}

impl Revision {
    pub fn revision(&self) -> Option<&SkillRevision> {
        match self {
            Revision::Revised(r) => Some(r),
            Revision::Refused(_) => None,
        }
    }
    pub fn into_revision(self) -> Option<SkillRevision> {
        match self {
            Revision::Revised(r) => Some(*r),
            Revision::Refused(_) => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReviseError {
    #[error("revision model call failed: {0}")]
    Model(String),
    #[error("model returned no usable revision: {0}")]
    BadRevision(String),
    #[error("current SKILL.md could not be read: {0}")]
    Skill(#[from] SkillError),
    /// The revised skill failed this workspace's own rules. Never silently
    /// written — a revision that breaks the skill is worse than no revision.
    #[error("revised skill is not valid: {}", .0.join("; "))]
    Invalid(Vec<String>),
}

/// Everything the reviser needs: the skill as it stands, the run that went
/// wrong, and why.
#[derive(Debug, Clone)]
pub struct RevisionRequest {
    /// Name of the skill to revise. Must match the frontmatter name.
    pub skill_name: String,
    /// The current `SKILL.md` in full (frontmatter + body).
    pub current_md: String,
    /// The run that went badly.
    pub episode: Episode,
    /// What went wrong, in the most specific terms available — a model
    /// self-report ("step 3 said `deploy.sh`, but the script is at
    /// `bin/deploy.sh`"), a stderr excerpt, or user feedback. This is the single
    /// highest-signal input; a vague complaint produces a vague revision.
    pub complaint: String,
}

impl RevisionRequest {
    pub fn new(
        skill_name: impl Into<String>,
        current_md: impl Into<String>,
        episode: Episode,
        complaint: impl Into<String>,
    ) -> Self {
        Self {
            skill_name: skill_name.into(),
            current_md: current_md.into(),
            episode,
            complaint: complaint.into(),
        }
    }

    /// Read `<skills_root>/<name>/SKILL.md` from disk. Reading only — the
    /// revision still has to be written back explicitly.
    pub fn from_dir(
        skills_root: &Path,
        skill_name: &str,
        episode: Episode,
        complaint: impl Into<String>,
    ) -> Result<Self, ReviseError> {
        harness_skills::validate_name(skill_name)?;
        let path = skills_root.join(skill_name).join("SKILL.md");
        let md = std::fs::read_to_string(&path)
            .map_err(|e| SkillError::Io(format!("{}: {e}", path.display())))?;
        Ok(Self::new(skill_name, md, episode, complaint))
    }
}

/// A proposed replacement for a skill. Not written anywhere until the caller
/// says so.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRevision {
    /// Unchanged from the original, always. See the module docs.
    pub name: String,
    /// Unchanged unless [`description_changed`](Self::description_changed).
    pub description: String,
    /// The revised markdown body.
    pub body: String,
    /// One sentence from the model on what it changed and why. Show this to
    /// whoever approves the revision.
    pub rationale: String,
    /// True when the model justified a new description.
    pub description_changed: bool,
    /// The bumped ledger, already reflecting this revision.
    pub ledger: RevisionLedger,
    /// Full frontmatter metadata, with every pre-existing key preserved and
    /// `experience` updated.
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl SkillRevision {
    pub fn manifest(&self) -> SkillManifest {
        skillmd::manifest(&self.name, &self.description, self.metadata.clone())
    }

    pub fn to_skill_md(&self) -> String {
        skillmd::render_skill_md(&self.manifest(), &self.body)
    }

    pub fn validate(&self) -> Result<(), ReviseError> {
        skillmd::validate_and_lint(&self.manifest(), &self.body).map_err(ReviseError::Invalid)
    }

    /// Opt-in persistence: overwrite `<skills_root>/<name>/SKILL.md`.
    /// `write_skill_md` restores the prior content if the new one won't load.
    pub fn write_to(&self, skills_root: &Path) -> Result<PathBuf, SkillError> {
        harness_skills::write_skill_md(skills_root, &self.name, &self.to_skill_md())
    }
}

/// Produces revisions for skills that misled a run.
pub struct SkillReviser {
    model: Arc<dyn Model>,
    policy: RevisePolicy,
    /// Test/host seam for "today". `None` = real UTC clock.
    today: Option<String>,
}

impl SkillReviser {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            policy: RevisePolicy::default(),
            today: None,
        }
    }

    pub fn with_policy(mut self, policy: RevisePolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_max_total_revisions(mut self, n: u32) -> Self {
        self.policy.max_total_revisions = n;
        self
    }

    pub fn with_max_revisions_per_day(mut self, n: u32) -> Self {
        self.policy.max_revisions_per_day = n;
        self
    }

    /// Override "today" (`YYYY-MM-DD`). For tests, and for hosts that keep their
    /// own clock (a scheduler replaying yesterday's episodes should date the
    /// revision when it happens, not when the episode did).
    pub fn with_today(mut self, day: impl Into<String>) -> Self {
        self.today = Some(day.into());
        self
    }

    pub fn policy(&self) -> &RevisePolicy {
        &self.policy
    }

    fn today(&self) -> String {
        self.today.clone().unwrap_or_else(skillmd::utc_today)
    }

    /// The cap and eligibility decision, with no model call.
    pub fn check(&self, req: &RevisionRequest) -> Result<RevisionGate, ReviseError> {
        let (manifest, _) = skillmd::split_skill_md(&req.current_md)?;
        Ok(self.check_parsed(req, &manifest))
    }

    fn check_parsed(&self, req: &RevisionRequest, manifest: &SkillManifest) -> RevisionGate {
        let p = &self.policy;
        if p.refuse_when_episode_succeeded && req.episode.succeeded() {
            return RevisionGate::Refuse(RefusalReason::EpisodeSucceeded);
        }
        if p.require_skill_in_episode
            && !req
                .episode
                .skills
                .iter()
                .any(|s| s == &req.skill_name || s == &manifest.name)
        {
            return RevisionGate::Refuse(RefusalReason::SkillNotUsedInEpisode {
                skill: req.skill_name.clone(),
            });
        }
        let ledger = RevisionLedger::from_manifest(manifest);
        if ledger.revision >= p.max_total_revisions {
            return RevisionGate::Refuse(RefusalReason::LifetimeCapReached {
                revision: ledger.revision,
                max: p.max_total_revisions,
            });
        }
        let today = self.today();
        let today_count = ledger.revisions_on(&today);
        if today_count >= p.max_revisions_per_day {
            return RevisionGate::Refuse(RefusalReason::DailyCapReached {
                today: today_count,
                max: p.max_revisions_per_day,
                date: today,
            });
        }
        RevisionGate::Revise {
            current_revision: ledger.revision,
            revisions_today: today_count,
        }
    }

    /// Check the caps, then (only if they allow) spend one model call to revise.
    ///
    /// Never writes. Hand the returned [`SkillRevision`] to a reviewer, or call
    /// [`SkillRevision::write_to`].
    pub async fn revise(&self, req: &RevisionRequest) -> Result<Revision, ReviseError> {
        let (manifest, body) = skillmd::split_skill_md(&req.current_md)?;
        match self.check_parsed(req, &manifest) {
            RevisionGate::Refuse(why) => return Ok(Revision::Refused(why)),
            RevisionGate::Revise { .. } => {}
        }

        let raw = llm::one_shot(
            &self.model,
            revise_prompt(&manifest, &body, req),
            self.policy.max_output_tokens,
        )
        .await
        .map_err(ReviseError::Model)?;
        let json = llm::extract_json(&raw).ok_or_else(|| {
            ReviseError::BadRevision(format!(
                "no JSON object in a {} char reply",
                raw.trim().len()
            ))
        })?;

        let new_body = llm::str_field(&json, "body").ok_or_else(|| {
            ReviseError::BadRevision("model returned no `body`; refusing to blank the skill".into())
        })?;
        if new_body.trim().len() < 50 {
            return Err(ReviseError::BadRevision(format!(
                "revised body is {} chars; refusing to replace a skill with a stub",
                new_body.trim().len()
            )));
        }

        // Description changes are opt-in and must be justified. Anything less —
        // a bare new string, or a change with an empty `why` — and the original
        // stands verbatim.
        let (description, description_changed) = match json.get("description_change") {
            Some(c) if c.is_object() => {
                match (llm::str_field(c, "new"), llm::str_field(c, "why")) {
                    (Some(new), Some(_why)) => (
                        skillmd::finalize_description(
                            &new,
                            &req.episode.situation,
                            &manifest.description,
                        ),
                        true,
                    ),
                    _ => (manifest.description.clone(), false),
                }
            }
            _ => (manifest.description.clone(), false),
        };

        let ledger = RevisionLedger::from_manifest(&manifest).bumped(&self.today());
        let mut metadata = manifest.metadata.clone();
        {
            // Preserve every other metadata key (e.g. `harness.kind`) by
            // updating in place rather than rebuilding.
            let mut tmp = manifest.clone();
            tmp.metadata = metadata;
            ledger.write_into(&mut tmp);
            metadata = tmp.metadata;
        }

        let revision = SkillRevision {
            name: manifest.name.clone(),
            description,
            body: new_body.trim().to_string(),
            rationale: llm::str_field(&json, "rationale")
                .unwrap_or_else(|| "(the model gave no rationale for this revision)".to_string()),
            description_changed,
            ledger,
            metadata,
        };
        revision.validate()?;
        Ok(Revision::Revised(Box::new(revision)))
    }
}

/// The single revision prompt.
fn revise_prompt(manifest: &SkillManifest, body: &str, req: &RevisionRequest) -> String {
    let tools = if req.episode.tools.is_empty() {
        "(none)".to_string()
    } else {
        req.episode.tools.join(" → ")
    };
    format!(
        "An AI agent followed the skill below and the run went wrong. Revise the skill so a \
         future agent following it would not hit the same problem.\n\n\
         ---- CURRENT SKILL ----\n\
         NAME (you may NOT change this): {name}\n\
         DESCRIPTION: {description}\n\
         BODY:\n{body}\n\
         ---- END SKILL ----\n\n\
         ---- THE RUN THAT WENT WRONG ----\n\
         SITUATION: {situation}\n\
         TOOLS CALLED, IN ORDER: {tools}\n\
         OUTCOME: {outcome}\n\
         WHAT WENT WRONG: {complaint}\n\
         ---- END RUN ----\n\n\
         Change the MINIMUM that fixes the problem. Keep every step that still holds, in the \
         same order and wording. Do not rewrite the skill's voice, do not add sections that \
         weren't there, and do not generalise away the concrete tool names and commands — those \
         are the useful part. If the failure was caused by the environment rather than by the \
         instructions, say so in the rationale and return the body essentially unchanged.\n\n\
         Reply with ONLY a JSON object, no prose around it:\n\
         {{\n\
         \x20 \"body\": \"the COMPLETE revised markdown body, with no YAML frontmatter\",\n\
         \x20 \"rationale\": \"one sentence: what you changed and why\",\n\
         \x20 \"description_change\": null\n\
         }}\n\n\
         Set `description_change` to null unless the current description now genuinely \
         mis-describes the skill. If it does, use \
         {{\"new\": \"...\", \"why\": \"...\"}} instead of null.",
        name = manifest.name,
        description = manifest.description,
        situation = req.episode.situation.trim(),
        outcome = req.episode.outcome.trim(),
        complaint = req.complaint.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_models::{MockModel, MockResponse};

    const ORIGINAL_DESCRIPTION: &str = "Ship the marketing site to production and confirm it is live. Use when the user asks \
         to deploy, ship, or release the site.";

    fn skill_md(ledger: Option<RevisionLedger>) -> String {
        let mut meta = BTreeMap::new();
        // A pre-existing, unrelated metadata key that must survive revision.
        meta.insert(
            "harness".to_string(),
            serde_json::json!({"kind": "inferential"}),
        );
        if let Some(l) = ledger {
            meta.insert(
                EXPERIENCE_METADATA_KEY.to_string(),
                serde_json::to_value(l).unwrap(),
            );
        }
        let m = skillmd::manifest("deploy-runbook", ORIGINAL_DESCRIPTION, meta);
        skillmd::render_skill_md(
            &m,
            "# Deploy Runbook\n\n## Steps\n\n1. Run deploy.sh.\n2. Check the site returns 200.\n",
        )
    }

    fn bad_episode() -> Episode {
        Episode::new(
            "deploy the marketing site to production",
            "deploy.sh: no such file or directory; site was never updated",
        )
        .with_tools(["shell", "read_file"])
        .with_skills(["deploy-runbook"])
        .with_success(false)
    }

    fn good_reply() -> String {
        serde_json::json!({
            "body": "# Deploy Runbook\n\n## Steps\n\n1. Run bin/deploy.sh (it lives under bin/, \
                     not the repo root).\n2. Check the site returns 200.\n",
            "rationale": "Corrected the script path: deploy.sh is at bin/deploy.sh.",
            "description_change": null
        })
        .to_string()
    }

    fn model(reply: &str) -> Arc<dyn Model> {
        Arc::new(MockModel::new().script(MockResponse::text(reply)))
    }

    fn reviser(reply: &str) -> SkillReviser {
        SkillReviser::new(model(reply)).with_today("2026-08-16")
    }

    fn request(md: String) -> RevisionRequest {
        RevisionRequest::new(
            "deploy-runbook",
            md,
            bad_episode(),
            "step 1 said `deploy.sh` but the script is at `bin/deploy.sh`",
        )
    }

    // ---- the happy path ---------------------------------------------------

    #[tokio::test]
    async fn reviser_preserves_name_and_description() {
        let r = reviser(&good_reply());
        let rev = r
            .revise(&request(skill_md(None)))
            .await
            .unwrap()
            .into_revision()
            .expect("should revise");

        assert_eq!(rev.name, "deploy-runbook", "name is never revised");
        assert_eq!(
            rev.description, ORIGINAL_DESCRIPTION,
            "description must be preserved byte for byte when unjustified"
        );
        assert!(!rev.description_changed);
        assert!(rev.body.contains("bin/deploy.sh"), "{}", rev.body);
        assert!(rev.rationale.contains("bin/deploy.sh"));

        // Ledger bumped, and unrelated metadata preserved.
        assert_eq!(rev.ledger.revision, 1);
        assert_eq!(rev.ledger.revised_on, "2026-08-16");
        assert_eq!(rev.ledger.revisions_today, 1);
        assert_eq!(rev.metadata["harness"]["kind"], "inferential");

        rev.validate().expect("revised skill must be valid");
    }

    #[tokio::test]
    async fn a_bare_description_string_without_a_why_is_ignored() {
        let reply = serde_json::json!({
            "body": "# Deploy Runbook\n\n## Steps\n\n1. Run bin/deploy.sh, which lives under bin/.\n",
            "rationale": "path fix",
            "description_change": {"new": "Something completely different."}
        })
        .to_string();
        let rev = reviser(&reply)
            .revise(&request(skill_md(None)))
            .await
            .unwrap()
            .into_revision()
            .unwrap();
        assert_eq!(rev.description, ORIGINAL_DESCRIPTION);
        assert!(!rev.description_changed);
    }

    #[tokio::test]
    async fn a_justified_description_change_is_taken() {
        let reply = serde_json::json!({
            "body": "# Deploy Runbook\n\n## Steps\n\n1. Run bin/deploy.sh, which lives under bin/.\n",
            "rationale": "the skill now covers staging too",
            "description_change": {
                "new": "Ship the site to production or staging and confirm it is live. Use when \
                        the user asks to deploy to any environment.",
                "why": "the runbook now handles staging, which the old description excluded"
            }
        })
        .to_string();
        let rev = reviser(&reply)
            .revise(&request(skill_md(None)))
            .await
            .unwrap()
            .into_revision()
            .unwrap();
        assert!(rev.description_changed);
        assert!(rev.description.contains("staging"));
        assert_eq!(rev.name, "deploy-runbook", "still never the name");
        rev.validate().unwrap();
    }

    #[tokio::test]
    async fn revision_round_trips_through_write_skill_md() {
        let root = std::env::temp_dir().join(format!(
            "harness-revise-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        harness_skills::write_skill_md(&root, "deploy-runbook", &skill_md(None)).unwrap();

        let req = RevisionRequest::from_dir(&root, "deploy-runbook", bad_episode(), "wrong path")
            .unwrap();
        let rev = reviser(&good_reply())
            .revise(&req)
            .await
            .unwrap()
            .into_revision()
            .unwrap();
        rev.write_to(&root).unwrap();

        let loaded = harness_skills::load_skill_dir(&root.join("deploy-runbook")).unwrap();
        assert_eq!(loaded.manifest().description, ORIGINAL_DESCRIPTION);
        assert!(loaded.body().contains("bin/deploy.sh"));
        let ledger = RevisionLedger::from_manifest(loaded.manifest());
        assert_eq!(ledger.revision, 1);
        assert_eq!(ledger.revised_on, "2026-08-16");

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- caps -------------------------------------------------------------

    /// The K+1th revision in a day is refused, and costs no model call.
    #[tokio::test]
    async fn daily_cap_refuses_the_k_plus_first() {
        let today = "2026-08-16";
        let at_cap = RevisionLedger {
            revision: 2,
            revised_on: today.into(),
            revisions_today: 2, // == default max_revisions_per_day
            origin: None,
        };
        let mock = Arc::new(MockModel::new().script(MockResponse::text(good_reply())));
        let r = SkillReviser::new(mock.clone() as Arc<dyn Model>).with_today(today);

        let req = request(skill_md(Some(at_cap)));
        assert_eq!(
            r.revise(&req).await.unwrap(),
            Revision::Refused(RefusalReason::DailyCapReached {
                today: 2,
                max: 2,
                date: today.into()
            })
        );
        assert_eq!(mock.call_count(), 0, "a refused revision costs no tokens");
    }

    /// …and the same skill is revisable again once the date rolls over.
    #[tokio::test]
    async fn daily_cap_resets_tomorrow() {
        let at_cap = RevisionLedger {
            revision: 2,
            revised_on: "2026-08-16".into(),
            revisions_today: 2,
            origin: None,
        };
        let r = SkillReviser::new(model(&good_reply())).with_today("2026-08-17");
        let rev = r
            .revise(&request(skill_md(Some(at_cap))))
            .await
            .unwrap()
            .into_revision()
            .expect("a new day gets a fresh budget");
        assert_eq!(rev.ledger.revision, 3);
        assert_eq!(rev.ledger.revised_on, "2026-08-17");
        assert_eq!(rev.ledger.revisions_today, 1, "counter reset, not carried");
    }

    #[tokio::test]
    async fn lifetime_cap_refuses_regardless_of_the_date() {
        let worn_out = RevisionLedger {
            revision: 10, // == default max_total_revisions
            revised_on: "2020-01-01".into(),
            revisions_today: 1,
            origin: Some("distilled".into()),
        };
        let r = reviser(&good_reply());
        assert_eq!(
            r.revise(&request(skill_md(Some(worn_out)))).await.unwrap(),
            Revision::Refused(RefusalReason::LifetimeCapReached {
                revision: 10,
                max: 10
            })
        );
    }

    /// Walk a skill from fresh to refused with the caps set low, proving the
    /// counters actually accumulate across separate revisions.
    #[tokio::test]
    async fn k_revisions_then_refusal() {
        const K: u32 = 3;
        let mut md = skill_md(None);
        for i in 1..=K {
            let r = SkillReviser::new(model(&good_reply()))
                .with_today("2026-08-16")
                .with_max_revisions_per_day(K)
                .with_max_total_revisions(100);
            let rev = r
                .revise(&request(md.clone()))
                .await
                .unwrap()
                .into_revision()
                .unwrap_or_else(|| panic!("revision {i} of {K} should be allowed"));
            assert_eq!(rev.ledger.revision, i);
            assert_eq!(rev.ledger.revisions_today, i);
            md = rev.to_skill_md();
        }
        let r = SkillReviser::new(model(&good_reply()))
            .with_today("2026-08-16")
            .with_max_revisions_per_day(K)
            .with_max_total_revisions(100);
        assert_eq!(
            r.revise(&request(md)).await.unwrap(),
            Revision::Refused(RefusalReason::DailyCapReached {
                today: K,
                max: K,
                date: "2026-08-16".into()
            }),
            "the K+1th revision in a day must be refused"
        );
    }

    // ---- eligibility ------------------------------------------------------

    #[tokio::test]
    async fn refuses_when_the_run_never_followed_the_skill() {
        let mut ep = bad_episode();
        ep.skills = vec!["some-other-skill".into()];
        let req = RevisionRequest::new("deploy-runbook", skill_md(None), ep, "broke");
        assert_eq!(
            reviser(&good_reply()).revise(&req).await.unwrap(),
            Revision::Refused(RefusalReason::SkillNotUsedInEpisode {
                skill: "deploy-runbook".into()
            })
        );
    }

    #[tokio::test]
    async fn refuses_when_the_run_succeeded() {
        let ep = bad_episode().with_success(true);
        let req = RevisionRequest::new("deploy-runbook", skill_md(None), ep, "nitpick");
        assert_eq!(
            reviser(&good_reply()).revise(&req).await.unwrap(),
            Revision::Refused(RefusalReason::EpisodeSucceeded)
        );
    }

    #[test]
    fn a_skill_with_no_ledger_is_revisable() {
        let r = reviser("");
        assert_eq!(
            r.check(&request(skill_md(None))).unwrap(),
            RevisionGate::Revise {
                current_revision: 0,
                revisions_today: 0
            }
        );
    }

    // ---- degenerate model replies ----------------------------------------

    #[tokio::test]
    async fn refuses_to_blank_a_skill() {
        for reply in [
            serde_json::json!({"rationale": "no idea"}).to_string(),
            serde_json::json!({"body": "  ", "rationale": "x"}).to_string(),
            serde_json::json!({"body": "# Deploy", "rationale": "x"}).to_string(),
        ] {
            let err = reviser(&reply)
                .revise(&request(skill_md(None)))
                .await
                .unwrap_err();
            assert!(
                matches!(err, ReviseError::BadRevision(_)),
                "expected BadRevision for {reply}, got {err}"
            );
        }
    }

    #[test]
    fn ledger_survives_a_frontmatter_round_trip() {
        let l = RevisionLedger {
            revision: 4,
            revised_on: "2026-08-16".into(),
            revisions_today: 2,
            origin: Some("distilled".into()),
        };
        let mut m = skillmd::manifest("x", "y", BTreeMap::new());
        l.write_into(&mut m);
        let md = skillmd::render_skill_md(&m, "body body body body body body body body");
        let (back, _) = skillmd::split_skill_md(&md).unwrap();
        assert_eq!(RevisionLedger::from_manifest(&back), l);
    }

    #[test]
    fn bumped_resets_on_a_new_day() {
        let l = RevisionLedger {
            revision: 1,
            revised_on: "2026-08-16".into(),
            revisions_today: 1,
            origin: None,
        };
        assert_eq!(l.bumped("2026-08-16").revisions_today, 2);
        assert_eq!(l.bumped("2026-08-17").revisions_today, 1);
        assert_eq!(l.bumped("2026-08-17").revision, 2, "lifetime never resets");
        assert_eq!(l.revisions_on("2026-08-17"), 0);
        assert_eq!(l.revisions_on("2026-08-16"), 1);
    }
}
