//! `SkillDistiller` — turn a run that worked into a reusable skill.
//!
//! The proactive half of the learning loop. `skill_manage` already lets a model
//! *choose* to write a skill; nothing ever asked it to. After a run ends, this
//! decides whether the episode is worth generalising and, if so, spends one
//! model call to draft an agentskills.io-compliant `SKILL.md`.
//!
//! ## The gate is arithmetic, not a model call
//!
//! Asking "was that worth remembering?" with an LLM every turn costs a call per
//! turn to answer "no" almost every time. [`DistillPolicy`] is instead four
//! cheap, inspectable predicates over the [`Episode`] plus a text-similarity
//! check against the skills already on disk. Only when all five pass does a
//! model see anything. On a chatty assistant the gate rejects nearly every run
//! for free.
//!
//! The default gate:
//!
//! | check | default | why |
//! |---|---|---|
//! | run succeeded | required | a procedure distilled from a failure teaches failure |
//! | ≥ N tool calls | 4 | fewer than that is an answer, not a procedure |
//! | ≥ M distinct tools | 2 | `shell` six times is one move repeated |
//! | situation length | 16 chars | "thanks!" has nothing to generalise |
//! | no similar skill | 0.6 overlap | see [`DistillPolicy::similarity_threshold`] |
//!
//! ## Nothing is written to disk
//!
//! [`SkillDistiller::distill`] returns a [`SkillDraft`]. A multi-tenant server
//! must scope the write per user, and many hosts want a human to approve a
//! machine-authored skill before agents start following it — neither is
//! possible if the distiller writes. [`SkillDraft::write_to`] and
//! [`SkillDistiller::distill_and_write`] are the opt-in convenience for hosts
//! that don't need either.
//!
//! ```ignore
//! let distiller = SkillDistiller::new(model.clone());
//! let existing = existing_skills_in(&user_skills_dir);
//! match distiller.distill(&episode, &existing).await? {
//!     Distillation::Drafted(draft) => queue_for_review(user_id, draft),
//!     Distillation::Skipped(why)   => tracing::debug!(?why, "no skill distilled"),
//! }
//! ```

use crate::episode::Episode;
use crate::llm;
use crate::revise::RevisionLedger;
use crate::skillmd;
use harness_core::{Model, SkillError, SkillManifest};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// When the distiller is allowed to spend a model call.
#[derive(Debug, Clone, PartialEq)]
pub struct DistillPolicy {
    /// Only distil runs the host marked successful. Unknown counts as "no" —
    /// see [`Episode::success`].
    pub require_success: bool,
    /// Minimum total tool calls. A run that answered from context reveals no
    /// procedure worth writing down.
    pub min_tool_calls: usize,
    /// Minimum *distinct* tools. Guards the degenerate case the raw count
    /// misses: six `shell` calls in a row is one move repeated, not a
    /// multi-step approach.
    pub min_distinct_tools: usize,
    /// Minimum trimmed length of the situation text.
    pub min_situation_chars: usize,
    /// Overlap-similarity above which an existing skill counts as "we already
    /// have this" — the defence against a chatty user accumulating 200
    /// near-identical skills.
    ///
    /// 0.6 on [`skillmd::overlap_similarity`] means "60% of the shorter text's
    /// topical tokens also appear in the longer one". Tuned against the
    /// realistic lopsided pair (a one-sentence situation vs a routing-cue-padded
    /// description); raise it toward 0.8 for a host that would rather have a
    /// near-duplicate than miss a real skill.
    pub similarity_threshold: f32,
    /// Output-token ceiling for the single distillation call.
    pub max_output_tokens: u32,
}

impl Default for DistillPolicy {
    fn default() -> Self {
        Self {
            require_success: true,
            min_tool_calls: 4,
            min_distinct_tools: 2,
            min_situation_chars: 16,
            similarity_threshold: 0.6,
            max_output_tokens: 1_200,
        }
    }
}

/// Why the gate declined to spend a model call. Every variant carries the
/// numbers behind the decision so a host can log or tune against it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SkipReason {
    /// The run failed, or nobody said whether it worked.
    NotSuccessful,
    TooFewToolCalls {
        got: usize,
        need: usize,
    },
    TooFewDistinctTools {
        got: usize,
        need: usize,
    },
    SituationTooShort {
        got: usize,
        need: usize,
    },
    /// A skill covering this already exists.
    DuplicateSkill {
        existing: String,
        similarity: f32,
        threshold: f32,
    },
    /// The drafted name collides with a skill already on disk. Distinct from
    /// `DuplicateSkill`: the *episode* looked novel, but the model landed on a
    /// name that's taken, which usually means the topics really do overlap.
    NameTaken {
        existing: String,
    },
}

/// Outcome of [`SkillDistiller::gate`].
#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// Worth a model call.
    Distill,
    Skip(SkipReason),
}

/// Outcome of [`SkillDistiller::distill`].
#[derive(Debug, Clone, PartialEq)]
pub enum Distillation {
    Drafted(SkillDraft),
    Skipped(SkipReason),
}

impl Distillation {
    pub fn draft(&self) -> Option<&SkillDraft> {
        match self {
            Distillation::Drafted(d) => Some(d),
            Distillation::Skipped(_) => None,
        }
    }
    pub fn into_draft(self) -> Option<SkillDraft> {
        match self {
            Distillation::Drafted(d) => Some(d),
            Distillation::Skipped(_) => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DistillError {
    #[error("distillation model call failed: {0}")]
    Model(String),
    #[error("model returned no usable draft: {0}")]
    BadDraft(String),
    /// The assembled draft failed this workspace's own skill rules. Reaching
    /// this is a bug in the assembler, not a warning about the model — the
    /// point of building the frontmatter in Rust is that it cannot happen.
    #[error("assembled draft is not a valid skill: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("writing skill: {0}")]
    Write(#[from] SkillError),
}

/// A skill that already exists, reduced to what duplicate detection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSkill {
    pub name: String,
    pub description: String,
}

impl ExistingSkill {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
    pub fn from_manifest(m: &SkillManifest) -> Self {
        Self::new(&m.name, &m.description)
    }
}

/// Every skill under `<root>/<name>/SKILL.md`, as duplicate-check input.
///
/// A missing or unreadable root yields an empty list rather than an error: "no
/// skills yet" is the normal state of a brand-new tenant, and it is also the
/// answer that makes the gate *permissive*, which is the right failure mode for
/// a first-ever skill.
pub fn existing_skills_in(root: &Path) -> Vec<ExistingSkill> {
    use harness_core::Skill as _;
    match harness_skills::scan_skills_root(root) {
        Ok(skills) => skills
            .iter()
            .map(|s| ExistingSkill::from_manifest(s.manifest()))
            .collect(),
        Err(e) => {
            tracing::debug!(root = %root.display(), error = %e, "no existing skills to compare against");
            Vec::new()
        }
    }
}

/// A drafted, not-yet-persisted skill.
///
/// Guaranteed by construction to pass `harness_skills::validate` and to produce
/// no Error/Warning lint findings — [`SkillDistiller::distill`] refuses to
/// return one that doesn't.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDraft {
    /// agentskills.io-legal name; also the directory name it must be written to.
    pub name: String,
    /// One-line description carrying a routing cue.
    pub description: String,
    /// Markdown body — no frontmatter.
    pub body: String,
    /// The episode situation this was distilled from, kept for provenance and
    /// for a human reviewing the draft.
    pub source_situation: String,
    /// Frontmatter `metadata`, pre-stamped with the revision ledger at 0.
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

impl SkillDraft {
    pub fn manifest(&self) -> SkillManifest {
        skillmd::manifest(&self.name, &self.description, self.metadata.clone())
    }

    /// The full `SKILL.md` text: frontmatter + body.
    pub fn to_skill_md(&self) -> String {
        skillmd::render_skill_md(&self.manifest(), &self.body)
    }

    /// Re-check the draft against `harness_skills`' validator and linter.
    pub fn validate(&self) -> Result<(), DistillError> {
        skillmd::validate_and_lint(&self.manifest(), &self.body).map_err(DistillError::Invalid)
    }

    /// Opt-in persistence: write `<skills_root>/<name>/SKILL.md`.
    ///
    /// Delegates to `harness_skills::write_skill_md`, which validates by loading
    /// the file back and rolls the write back if it doesn't load — so a bad
    /// draft cannot leave a broken skill behind.
    pub fn write_to(&self, skills_root: &Path) -> Result<PathBuf, SkillError> {
        harness_skills::write_skill_md(skills_root, &self.name, &self.to_skill_md())
    }
}

/// Decides whether a finished run is worth a skill, and drafts it.
pub struct SkillDistiller {
    model: Arc<dyn Model>,
    policy: DistillPolicy,
}

impl SkillDistiller {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            policy: DistillPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: DistillPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_min_tool_calls(mut self, n: usize) -> Self {
        self.policy.min_tool_calls = n;
        self
    }

    pub fn with_similarity_threshold(mut self, t: f32) -> Self {
        self.policy.similarity_threshold = t;
        self
    }

    pub fn policy(&self) -> &DistillPolicy {
        &self.policy
    }

    /// The whole trigger decision, with no model call and no I/O.
    ///
    /// Checks run cheapest-first and return the *first* failure, so the reason a
    /// host logs is the most fundamental one rather than an incidental later
    /// check.
    pub fn gate(&self, ep: &Episode, existing: &[ExistingSkill]) -> GateDecision {
        let p = &self.policy;
        if p.require_success && !ep.succeeded() {
            return GateDecision::Skip(SkipReason::NotSuccessful);
        }
        if ep.tools.len() < p.min_tool_calls {
            return GateDecision::Skip(SkipReason::TooFewToolCalls {
                got: ep.tools.len(),
                need: p.min_tool_calls,
            });
        }
        let distinct = ep
            .tools
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        if distinct < p.min_distinct_tools {
            return GateDecision::Skip(SkipReason::TooFewDistinctTools {
                got: distinct,
                need: p.min_distinct_tools,
            });
        }
        let situation_len = ep.situation.trim().chars().count();
        if situation_len < p.min_situation_chars {
            return GateDecision::Skip(SkipReason::SituationTooShort {
                got: situation_len,
                need: p.min_situation_chars,
            });
        }
        if let Some((name, sim)) = self.nearest(&ep.situation, existing) {
            return GateDecision::Skip(SkipReason::DuplicateSkill {
                existing: name,
                similarity: sim,
                threshold: p.similarity_threshold,
            });
        }
        GateDecision::Distill
    }

    /// The existing skill most similar to `text`, if any clears the threshold.
    ///
    /// Compares against `name + description` because a good description already
    /// enumerates the situations the skill covers — the exact thing an episode's
    /// situation is a sample of.
    ///
    /// **Why the skills registry and not `ExperienceStore::recall`.** Recalling
    /// similar *episodes* answers a different question: "have I done this
    /// before?" A chatty user does the same task weekly, so on the second run
    /// episode-similarity would fire and suppress the very first distillation —
    /// while doing nothing whatsoever to stop the 200 near-identical skills this
    /// check exists to prevent. The only source of truth for "does a skill for
    /// this already exist" is the skills on disk.
    fn nearest(&self, text: &str, existing: &[ExistingSkill]) -> Option<(String, f32)> {
        existing
            .iter()
            .map(|s| {
                let hay = format!("{} {}", s.name.replace('-', " "), s.description);
                (s.name.clone(), skillmd::overlap_similarity(text, &hay))
            })
            .filter(|(_, sim)| *sim >= self.policy.similarity_threshold)
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// Gate, then (only if it passes) spend one model call to draft a skill.
    ///
    /// Never writes to disk. `existing` is the duplicate-suppression corpus —
    /// use [`existing_skills_in`] to build it from a skills root, or pass the
    /// caller's own per-tenant view.
    pub async fn distill(
        &self,
        ep: &Episode,
        existing: &[ExistingSkill],
    ) -> Result<Distillation, DistillError> {
        match self.gate(ep, existing) {
            GateDecision::Skip(why) => return Ok(Distillation::Skipped(why)),
            GateDecision::Distill => {}
        }

        let raw = llm::one_shot(
            &self.model,
            distill_prompt(ep),
            self.policy.max_output_tokens,
        )
        .await
        .map_err(DistillError::Model)?;
        let json = llm::extract_json(&raw).ok_or_else(|| {
            DistillError::BadDraft(format!(
                "no JSON object in a {} char reply",
                raw.trim().len()
            ))
        })?;

        let draft = assemble_draft(ep, &json)?;

        // Second duplicate pass, now against what the model actually produced.
        // The episode's situation can read as novel while the model, seeing the
        // same task through a generalising lens, lands on a skill that already
        // exists — this catches that.
        if let Some(clash) = existing.iter().find(|s| s.name == draft.name) {
            return Ok(Distillation::Skipped(SkipReason::NameTaken {
                existing: clash.name.clone(),
            }));
        }
        let drafted_text = format!("{} {}", draft.name.replace('-', " "), draft.description);
        if let Some((name, sim)) = self.nearest(&drafted_text, existing) {
            return Ok(Distillation::Skipped(SkipReason::DuplicateSkill {
                existing: name,
                similarity: sim,
                threshold: self.policy.similarity_threshold,
            }));
        }

        draft.validate()?;
        Ok(Distillation::Drafted(draft))
    }

    /// Convenience for hosts that need neither per-tenant scoping nor a human
    /// gate: scan `skills_root` for duplicates, distil, and write on success.
    ///
    /// Returns the written path, or `None` when the gate declined.
    pub async fn distill_and_write(
        &self,
        ep: &Episode,
        skills_root: &Path,
    ) -> Result<Option<PathBuf>, DistillError> {
        let existing = existing_skills_in(skills_root);
        match self.distill(ep, &existing).await? {
            Distillation::Skipped(why) => {
                tracing::debug!(?why, "no skill distilled");
                Ok(None)
            }
            Distillation::Drafted(draft) => Ok(Some(draft.write_to(skills_root)?)),
        }
    }
}

/// The single distillation prompt. Asks for *fields*, never for a finished
/// SKILL.md — see [`crate::skillmd`] for why that inversion is what makes the
/// output guaranteed-valid.
fn distill_prompt(ep: &Episode) -> String {
    let tools = if ep.tools.is_empty() {
        "(none)".to_string()
    } else {
        ep.tools.join(" → ")
    };
    format!(
        "You are distilling procedural memory for an AI agent.\n\n\
         A task just finished successfully. Turn it into a REUSABLE skill: a procedure a \
         future agent can follow for the same CLASS of task. Generalise. Do not write a \
         diary entry about this one run, and do not bake in one-off values (this ticket \
         number, this filename) — describe the role each played instead.\n\n\
         ---- THE RUN ----\n\
         SITUATION: {situation}\n\
         TOOLS CALLED, IN ORDER: {tools}\n\
         OUTCOME: {outcome}\n\
         ---- END ----\n\n\
         Reply with ONLY a JSON object, no prose around it:\n\
         {{\n\
         \x20 \"name\": \"lowercase-hyphenated, at most 5 words, class-level (e.g. \
         \\\"deploy-runbook\\\", never \\\"fix-bug-1234\\\")\",\n\
         \x20 \"title\": \"short human title, Title Case\",\n\
         \x20 \"description\": \"1-2 sentences: what the skill does AND when to use it. \
         Begin the second half with 'Use when'.\",\n\
         \x20 \"steps\": [\"imperative step\", \"...\"],\n\
         \x20 \"pitfalls\": [\"a gotcha a future agent would otherwise hit\", \"...\"]\n\
         }}\n\n\
         Rules: 3 to 10 steps. Name the concrete tool or command in a step where one \
         applies. `pitfalls` may be empty if you have nothing honest to put there.",
        situation = ep.situation.trim(),
        outcome = ep.outcome.trim(),
    )
}

/// Turn the model's JSON fields into a valid draft, repairing anything the spec
/// cares about rather than rejecting it.
fn assemble_draft(ep: &Episode, json: &serde_json::Value) -> Result<SkillDraft, DistillError> {
    let raw_name = llm::str_field(json, "name").unwrap_or_default();
    let mut name = skillmd::slugify(&raw_name);
    if name.is_empty() {
        // Fall back to the situation rather than failing: a model that gave us
        // good steps under a useless name is still worth a skill.
        name = skillmd::slugify(&ep.situation);
    }
    if name.is_empty() {
        return Err(DistillError::BadDraft(
            "no usable skill name could be derived from the model reply or the episode".into(),
        ));
    }

    let steps = llm::str_list(json, "steps");
    if steps.is_empty() {
        return Err(DistillError::BadDraft(
            "model returned no steps; a skill with no procedure is not a skill".into(),
        ));
    }
    let pitfalls = llm::str_list(json, "pitfalls");

    let description = skillmd::finalize_description(
        &llm::str_field(json, "description").unwrap_or_default(),
        &ep.situation,
        &name.replace('-', " "),
    );

    let title = llm::str_field(json, "title").unwrap_or_else(|| title_from_slug(&name));
    let body = render_body(&title, &description, &ep.situation, &steps, &pitfalls);

    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert(
        crate::revise::EXPERIENCE_METADATA_KEY.to_string(),
        serde_json::to_value(RevisionLedger::distilled()).unwrap_or(serde_json::Value::Null),
    );

    Ok(SkillDraft {
        name,
        description,
        body,
        source_situation: ep.situation.trim().to_string(),
        metadata,
    })
}

fn title_from_slug(slug: &str) -> String {
    slug.split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Assemble the markdown body.
///
/// The `## When this applies` section is not decoration: it carries the source
/// situation into the file (provenance a human reviewer needs) and it is what
/// guarantees the body clears lint R3's 50-character floor even when a model
/// returns three terse steps.
fn render_body(
    title: &str,
    description: &str,
    situation: &str,
    steps: &[String],
    pitfalls: &[String],
) -> String {
    let mut b = format!("# {title}\n\n{description}\n\n## When this applies\n\n");
    b.push_str(situation.trim());
    b.push_str("\n\n## Steps\n\n");
    for (i, s) in steps.iter().enumerate() {
        // Strip any numbering the model already added, so we don't emit "1. 1. …".
        let s = s
            .trim()
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')', '-', ' ']);
        b.push_str(&format!("{}. {}\n", i + 1, s.trim()));
    }
    if !pitfalls.is_empty() {
        b.push_str("\n## Pitfalls\n\n");
        for p in pitfalls {
            b.push_str(&format!(
                "- {}\n",
                p.trim().trim_start_matches(['-', '*']).trim()
            ));
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_models::{MockModel, MockResponse};

    /// A well-formed distillation reply.
    fn good_reply() -> String {
        serde_json::json!({
            "name": "Deploy Runbook",
            "title": "Deploy Runbook",
            "description": "Ship the marketing site to production and confirm it is live.",
            "steps": [
                "Read deploy.toml to confirm the target environment.",
                "Run `cargo build --release` and check it exits zero.",
                "Run deploy.sh with the target from step 1.",
                "Fetch the public URL and assert it returns 200."
            ],
            "pitfalls": ["deploy.sh is not idempotent; do not re-run it after a partial failure"]
        })
        .to_string()
    }

    fn complex_episode() -> Episode {
        Episode::new(
            "the user asked to deploy the marketing site to production and verify it is live",
            "built, ran deploy.sh, confirmed the site returns 200",
        )
        .with_tools([
            "read_file",
            "shell",
            "write_file",
            "shell",
            "web_fetch",
            "read_file",
        ])
        .with_success(true)
    }

    fn model(reply: &str) -> Arc<dyn Model> {
        Arc::new(MockModel::new().script(MockResponse::text(reply)))
    }

    fn tmp_root(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "harness-distill-{tag}-{}-{n}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // ---- gate -------------------------------------------------------------

    #[test]
    fn gate_rejects_a_trivial_one_tool_episode() {
        let d = SkillDistiller::new(model(""));
        let ep = Episode::new("what time is it in Tokyo?", "answered: 3pm JST")
            .with_tools(["get_time"])
            .with_success(true);
        assert_eq!(
            d.gate(&ep, &[]),
            GateDecision::Skip(SkipReason::TooFewToolCalls { got: 1, need: 4 })
        );
    }

    #[test]
    fn gate_accepts_a_six_tool_successful_episode() {
        let d = SkillDistiller::new(model(""));
        assert_eq!(d.gate(&complex_episode(), &[]), GateDecision::Distill);
    }

    #[test]
    fn gate_rejects_a_failed_or_unmarked_run() {
        let d = SkillDistiller::new(model(""));
        let mut ep = complex_episode();
        ep.success = Some(false);
        assert_eq!(
            d.gate(&ep, &[]),
            GateDecision::Skip(SkipReason::NotSuccessful)
        );
        ep.success = None;
        assert_eq!(
            d.gate(&ep, &[]),
            GateDecision::Skip(SkipReason::NotSuccessful),
            "an unconfirmed run must be treated as not-successful"
        );
    }

    #[test]
    fn gate_rejects_one_move_repeated() {
        let d = SkillDistiller::new(model(""));
        let ep = Episode::new(
            "grep the repo for every TODO comment and list them",
            "listed 41 TODOs",
        )
        .with_tools(["shell", "shell", "shell", "shell", "shell"])
        .with_success(true);
        assert_eq!(
            d.gate(&ep, &[]),
            GateDecision::Skip(SkipReason::TooFewDistinctTools { got: 1, need: 2 })
        );
    }

    #[tokio::test]
    async fn gate_rejection_spends_no_model_call() {
        let mock = Arc::new(MockModel::new().script(MockResponse::text(good_reply())));
        let d = SkillDistiller::new(mock.clone() as Arc<dyn Model>);
        let trivial = Episode::new("hi", "hello").with_success(true);
        let out = d.distill(&trivial, &[]).await.unwrap();
        assert!(matches!(out, Distillation::Skipped(_)));
        assert_eq!(
            mock.call_count(),
            0,
            "the gate exists so that a boring run costs zero tokens"
        );
    }

    // ---- drafting ---------------------------------------------------------

    #[tokio::test]
    async fn distilled_draft_passes_validate_and_lint() {
        let d = SkillDistiller::new(model(&good_reply()));
        let draft = d
            .distill(&complex_episode(), &[])
            .await
            .unwrap()
            .into_draft()
            .expect("gate should pass");

        // The model's "Deploy Runbook" became a spec-legal name.
        assert_eq!(draft.name, "deploy-runbook");
        harness_skills::validate_name(&draft.name).unwrap();

        // Straight through this workspace's own validator + linter.
        let manifest = draft.manifest();
        harness_skills::validate(&manifest).expect("validate");
        let findings = harness_skills::lint_skills(&[harness_skills::FileSkill::new(
            manifest,
            draft.body.clone(),
            Vec::new(),
        )]);
        let loud: Vec<_> = findings
            .iter()
            .filter(|f| f.severity != harness_skills::LintSeverity::Info)
            .collect();
        assert!(loud.is_empty(), "lint complained: {loud:#?}");
        assert!(findings.is_empty(), "even info findings: {findings:#?}");
    }

    #[tokio::test]
    async fn draft_round_trips_through_write_skill_md() {
        let root = tmp_root("roundtrip");
        let d = SkillDistiller::new(model(&good_reply()));
        let draft = d
            .distill(&complex_episode(), &[])
            .await
            .unwrap()
            .into_draft()
            .unwrap();

        // write_skill_md validates by loading the file back, and rolls back if
        // it doesn't load — so a successful write IS the round-trip proof.
        let path = draft
            .write_to(&root)
            .expect("write_skill_md must accept it");
        assert!(path.exists());

        let loaded = harness_skills::load_skill_dir(&root.join(&draft.name)).unwrap();
        assert_eq!(loaded.manifest().name, draft.name);
        assert_eq!(loaded.manifest().description, draft.description);
        assert!(loaded.body().contains("## Steps"));
        assert!(loaded.body().contains("1. Read deploy.toml"));
        // The revision ledger survived the YAML round-trip at 0.
        let ledger = RevisionLedger::from_manifest(loaded.manifest());
        assert_eq!(ledger.revision, 0);
        assert_eq!(ledger.origin.as_deref(), Some("distilled"));

        // And it is discoverable by a plain registry scan.
        let scanned = existing_skills_in(&root);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].name, "deploy-runbook");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_terse_model_reply_still_yields_a_lint_clean_skill() {
        // Minimum viable reply: no title, no pitfalls, a description far too
        // short and with no routing cue. The assembler must repair all of it.
        let reply = serde_json::json!({
            "name": "site-deploy",
            "description": "Deploys.",
            "steps": ["1. build", "2. ship"]
        })
        .to_string();
        let d = SkillDistiller::new(model(&reply));
        let draft = d
            .distill(&complex_episode(), &[])
            .await
            .unwrap()
            .into_draft()
            .unwrap();
        draft.validate().expect("repaired draft must be valid");
        assert!(draft.description.to_ascii_lowercase().contains("use when"));
        assert!(draft.description.len() >= 60);
        // Model-supplied numbering must not be doubled up.
        assert!(draft.body.contains("1. build"), "{}", draft.body);
        assert!(!draft.body.contains("1. 1."), "{}", draft.body);
    }

    #[tokio::test]
    async fn a_reply_with_no_steps_is_an_error_not_a_skill() {
        let reply = serde_json::json!({"name": "x", "description": "y"}).to_string();
        let d = SkillDistiller::new(model(&reply));
        let err = d.distill(&complex_episode(), &[]).await.unwrap_err();
        assert!(matches!(err, DistillError::BadDraft(_)), "{err}");
    }

    #[tokio::test]
    async fn a_non_json_reply_is_an_error() {
        let d = SkillDistiller::new(model("I'm sorry, I can't help with that."));
        let err = d.distill(&complex_episode(), &[]).await.unwrap_err();
        assert!(matches!(err, DistillError::BadDraft(_)), "{err}");
    }

    // ---- duplicate suppression -------------------------------------------

    #[tokio::test]
    async fn near_duplicate_suppression_fires_before_the_model_call() {
        let mock = Arc::new(MockModel::new().script(MockResponse::text(good_reply())));
        let d = SkillDistiller::new(mock.clone() as Arc<dyn Model>);
        let existing = vec![ExistingSkill::new(
            "site-deployment",
            "Deploy the marketing site to production and verify it. Use when the user asks to \
             ship, release, or push the site live.",
        )];

        let out = d.distill(&complex_episode(), &existing).await.unwrap();
        match out {
            Distillation::Skipped(SkipReason::DuplicateSkill {
                existing,
                similarity,
                threshold,
            }) => {
                assert_eq!(existing, "site-deployment");
                assert!(similarity >= threshold, "{similarity} < {threshold}");
            }
            other => panic!("expected duplicate suppression, got {other:?}"),
        }
        assert_eq!(mock.call_count(), 0, "a duplicate must cost zero tokens");
    }

    #[tokio::test]
    async fn an_unrelated_existing_skill_does_not_suppress() {
        let d = SkillDistiller::new(model(&good_reply()));
        let existing = vec![ExistingSkill::new(
            "invoice-extraction",
            "Pull line items out of a scanned PDF invoice into a spreadsheet row. Use when the \
             user uploads a supplier invoice.",
        )];
        let out = d.distill(&complex_episode(), &existing).await.unwrap();
        assert!(out.draft().is_some(), "{out:?}");
    }

    #[tokio::test]
    async fn a_taken_name_is_caught_after_drafting() {
        // The situation is worded so it does NOT clear the pre-call similarity
        // check, but the model still names the skill `deploy-runbook`.
        let d = SkillDistiller::new(model(&good_reply()));
        let existing = vec![ExistingSkill::new(
            "deploy-runbook",
            "An unrelated-sounding description about quarterly invoice reconciliation \
             spreadsheets. Use when reconciling ledgers.",
        )];
        let out = d.distill(&complex_episode(), &existing).await.unwrap();
        assert_eq!(
            out,
            Distillation::Skipped(SkipReason::NameTaken {
                existing: "deploy-runbook".into()
            })
        );
    }

    #[tokio::test]
    async fn a_chatty_user_accumulates_exactly_one_skill() {
        // The scenario the whole duplicate check exists for: the same task,
        // five times, against a real skills directory.
        let root = tmp_root("chatty");
        for i in 0..5 {
            let d = SkillDistiller::new(model(&good_reply()));
            let ep = Episode::new(
                format!(
                    "the user asked to deploy the marketing site to production and verify it is \
                     live (request {i})"
                ),
                "built, ran deploy.sh, confirmed 200",
            )
            .with_tools(["read_file", "shell", "write_file", "shell", "web_fetch"])
            .with_success(true);
            let _ = d.distill_and_write(&ep, &root).await.unwrap();
        }
        let skills = existing_skills_in(&root);
        assert_eq!(
            skills.len(),
            1,
            "five near-identical runs must not mint five skills: {skills:#?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn distill_and_write_returns_none_when_gated_out() {
        let root = tmp_root("gated");
        let d = SkillDistiller::new(model(&good_reply()));
        let ep = Episode::new("hi there friend", "hello").with_success(true);
        assert!(d.distill_and_write(&ep, &root).await.unwrap().is_none());
        assert!(existing_skills_in(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
