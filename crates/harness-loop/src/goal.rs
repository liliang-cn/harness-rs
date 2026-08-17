//! A goal: the run itself, written down and survivable.
//!
//! A spec and a goal look alike on the page and are not the same object. A spec
//! is design authority — you think hard, you write it, you hand it to the model
//! as context. It is a bigger prompt. Nothing about it touches the runtime, so
//! when the process dies there is nothing to come back to: the file is still
//! there and the run is gone.
//!
//! A goal is *bound to the run*. It has an id, it holds which phase is in
//! flight, and it is on disk before the first turn, so a crash, a laptop lid,
//! or a deliberate stop leaves something to resume rather than something to
//! re-derive. That is the whole distinction, and it is why this lives beside
//! [`crate::seal`] and [`crate::receipt`] rather than in a docs folder: goal
//! says what, seal says what may not move while it happens, receipt says what
//! came of it.
//!
//! **Context is referenced, not inlined.** [`Goal::context`] holds paths. A
//! goal that embeds the material it points at is a prompt again — it goes stale
//! against the files it copied, and it grows until it is the thing you were
//! trying to avoid re-reading.
//!
//! **Phases exist to bound review, not the model.** The useful question is not
//! "how long may this run" but "how many commits am I willing to read". Ten to
//! twenty phases is the range that has worked; nothing here enforces it,
//! because the right number is a property of the work.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Current [`Goal::schema`].
pub const SCHEMA: &str = "harness.goal.v1";

/// Where one phase stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    Running,
    Done,
    /// Attempted and did not hold. Kept rather than reset to `Pending` so a
    /// resumed goal can tell "not started" from "tried once and failed",
    /// which are different things to hand back to a model.
    Failed,
}

/// One reviewable chunk of the objective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    pub title: String,
    #[serde(default = "pending")]
    pub status: PhaseStatus,
    /// What happened. On a failure this is what gets handed back on resume.
    #[serde(default)]
    pub note: String,
}

fn pending() -> PhaseStatus {
    PhaseStatus::Pending
}

impl Phase {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            status: PhaseStatus::Pending,
            note: String::new(),
        }
    }
}

/// A durable, resumable objective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub schema: String,
    /// Stable across resumes. The thing a log line can point at.
    pub id: String,
    /// One outcome. Not a list — a goal that wants three things is three goals,
    /// and the single objective is what makes "what was this run for?"
    /// answerable months later.
    pub objective: String,
    /// Paths to read, not their contents. See the module docs.
    #[serde(default)]
    pub context: Vec<PathBuf>,
    /// How to work: cautious, exploratory, ship-it. Free text, because the
    /// useful values differ per team and an enum would just be ignored.
    #[serde(default)]
    pub posture: String,
    /// What must not change. Stated separately from the objective because the
    /// model reads the objective as an instruction and these as a boundary.
    #[serde(default)]
    pub invariants: Vec<String>,
    #[serde(default)]
    pub phases: Vec<Phase>,
    /// How completion is checked, in words. The executable form is an
    /// [`crate::Acceptance`]; this is what a human reads to know what that
    /// check is supposed to be enforcing.
    #[serde(default)]
    pub verify: String,
    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub updated_ms: i64,
}

impl Goal {
    pub fn new(id: impl Into<String>, objective: impl Into<String>, now_ms: i64) -> Self {
        Self {
            schema: SCHEMA.into(),
            id: id.into(),
            objective: objective.into(),
            context: Vec::new(),
            posture: String::new(),
            invariants: Vec::new(),
            phases: Vec::new(),
            verify: String::new(),
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    pub fn with_context<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.context = paths.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_posture(mut self, p: impl Into<String>) -> Self {
        self.posture = p.into();
        self
    }

    pub fn with_invariants<I, S>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.invariants = items.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_phases<I, S>(mut self, titles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.phases = titles.into_iter().map(|t| Phase::new(t)).collect();
        self
    }

    pub fn with_verify(mut self, v: impl Into<String>) -> Self {
        self.verify = v.into();
        self
    }

    /// The phase to work on: the one running, else the first not yet done.
    ///
    /// A `Failed` phase is returned again rather than skipped — the run stopped
    /// there for a reason and resuming past it would quietly abandon work the
    /// goal still claims to want.
    pub fn current(&self) -> Option<(usize, &Phase)> {
        self.phases
            .iter()
            .enumerate()
            .find(|(_, p)| p.status == PhaseStatus::Running)
            .or_else(|| {
                self.phases
                    .iter()
                    .enumerate()
                    .find(|(_, p)| matches!(p.status, PhaseStatus::Pending | PhaseStatus::Failed))
            })
    }

    /// Every phase is `Done`. A goal with no phases is never complete: it has
    /// not been broken down, so there is nothing to have finished.
    pub fn complete(&self) -> bool {
        !self.phases.is_empty() && self.phases.iter().all(|p| p.status == PhaseStatus::Done)
    }

    /// Mark the current phase as in flight.
    pub fn start_current(&mut self, now_ms: i64) -> Option<usize> {
        let i = self.current()?.0;
        self.phases[i].status = PhaseStatus::Running;
        self.updated_ms = now_ms;
        Some(i)
    }

    pub fn finish(&mut self, i: usize, note: impl Into<String>, now_ms: i64) {
        if let Some(p) = self.phases.get_mut(i) {
            p.status = PhaseStatus::Done;
            p.note = note.into();
            self.updated_ms = now_ms;
        }
    }

    pub fn fail(&mut self, i: usize, why: impl Into<String>, now_ms: i64) {
        if let Some(p) = self.phases.get_mut(i) {
            p.status = PhaseStatus::Failed;
            p.note = why.into();
            self.updated_ms = now_ms;
        }
    }

    /// Render the brief for the current phase.
    ///
    /// Includes the objective every time. A model resuming at phase 9 has none
    /// of the earlier conversation, and a phase title on its own ("wire the
    /// handler") is not a task — it is a reminder to someone who already knew.
    pub fn brief(&self) -> String {
        let mut s = format!("# Objective\n{}\n", self.objective.trim());

        if !self.posture.is_empty() {
            s.push_str(&format!("\n# How to work\n{}\n", self.posture.trim()));
        }
        if !self.context.is_empty() {
            s.push_str("\n# Read first\n");
            for p in &self.context {
                s.push_str(&format!("- {}\n", p.display()));
            }
        }
        if !self.invariants.is_empty() {
            s.push_str("\n# Do not change\n");
            for i in &self.invariants {
                s.push_str(&format!("- {i}\n"));
            }
        }
        if !self.phases.is_empty() {
            let done = self
                .phases
                .iter()
                .filter(|p| p.status == PhaseStatus::Done)
                .count();
            s.push_str(&format!(
                "\n# Phase {} of {}\n",
                done + 1,
                self.phases.len()
            ));
            match self.current() {
                Some((_, p)) => {
                    s.push_str(&format!("{}\n", p.title));
                    // The previous attempt, if there was one. This is the whole
                    // value of keeping `Failed` distinct from `Pending`.
                    if p.status == PhaseStatus::Failed && !p.note.is_empty() {
                        s.push_str(&format!(
                            "\nThis phase was attempted and did not hold: {}\n\
                             Address that before anything else.\n",
                            p.note
                        ));
                    }
                }
                None => s.push_str("All phases are done.\n"),
            }
        }
        if !self.verify.is_empty() {
            s.push_str(&format!("\n# Done when\n{}\n", self.verify.trim()));
        }
        s
    }
}

/// Goals on disk, one JSON file per id.
///
/// A directory of plain files rather than a database: the same reasoning as
/// `FileMemory` — greppable, diffable, and a goal you can open in an editor
/// mid-run is worth more than one you have to query for.
pub struct GoalStore {
    dir: PathBuf,
}

impl GoalStore {
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, id: &str) -> PathBuf {
        // Ids reach the filesystem, so they get the same treatment every other
        // externally-supplied path segment gets here: anything that is not a
        // clean identifier byte becomes one that is.
        let safe: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    pub fn save(&self, g: &Goal) -> std::io::Result<()> {
        // Write-rename: a goal half-written by a process that died is worse
        // than no goal, because resume would read it and believe it.
        let p = self.path(&g.id);
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(g).unwrap_or_default())?;
        std::fs::rename(&tmp, &p)
    }

    pub fn load(&self, id: &str) -> std::io::Result<Goal> {
        let s = std::fs::read_to_string(self.path(id))?;
        serde_json::from_str(&s).map_err(std::io::Error::other)
    }

    /// Ids of every goal that is not finished, oldest first — what "resume"
    /// offers you.
    pub fn unfinished(&self) -> Vec<Goal> {
        let mut out: Vec<Goal> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<Goal>(&s).ok())
            .filter(|g| !g.complete())
            .collect();
        out.sort_by_key(|g| g.created_ms);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal() -> Goal {
        Goal::new("g1", "Port the deploy from Netlify to Azure", 1000)
            .with_context(["docs/deploy.md", "infra/"])
            .with_posture("Cautious. Prefer reversible steps.")
            .with_invariants(["the public API shape", "the database schema"])
            .with_phases(["stand up the Azure app", "move the DNS", "retire Netlify"])
            .with_verify("the five gold queries return the same answers as production")
    }

    fn store() -> (GoalStore, PathBuf) {
        let d = std::env::temp_dir().join(format!(
            "harness-goal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        (GoalStore::open(&d).unwrap(), d)
    }

    #[test]
    fn a_goal_survives_the_process_that_started_it() {
        // The property that makes it a goal and not a spec.
        let (s, d) = store();
        let mut g = goal();
        let i = g.start_current(1001).unwrap();
        g.finish(i, "app service created", 1002);
        s.save(&g).unwrap();

        let back = s.load("g1").unwrap();
        assert_eq!(back, g);
        assert_eq!(back.current().unwrap().1.title, "move the DNS");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_phase_is_retried_not_skipped() {
        let mut g = goal();
        let i = g.start_current(1).unwrap();
        g.fail(i, "the app service quota was exhausted", 2);
        let (j, p) = g.current().unwrap();
        assert_eq!(j, i, "resume must land back on the phase that failed");
        assert_eq!(p.status, PhaseStatus::Failed);
        // And the brief has to say so, or the model repeats the failure.
        let b = g.brief();
        assert!(b.contains("did not hold"), "{b}");
        assert!(b.contains("quota was exhausted"), "{b}");
    }

    #[test]
    fn the_brief_restates_the_objective_at_every_phase() {
        // A model resuming at phase 3 has none of the earlier conversation.
        let mut g = goal();
        for k in 0..2 {
            let i = g.start_current(k).unwrap();
            g.finish(i, "ok", k);
        }
        let b = g.brief();
        assert!(b.contains("Port the deploy from Netlify to Azure"));
        assert!(b.contains("Phase 3 of 3"));
        assert!(b.contains("retire Netlify"));
        assert!(b.contains("the database schema"), "invariants must carry");
    }

    #[test]
    fn context_is_listed_as_paths_not_pasted_in() {
        let g = goal();
        let b = g.brief();
        assert!(b.contains("docs/deploy.md"));
        // If someone later "helpfully" inlines the file, this catches it: the
        // brief must stay a set of pointers.
        assert!(b.len() < 800, "the brief grew into a prompt:\n{b}");
    }

    #[test]
    fn a_goal_with_no_phases_is_never_complete() {
        // Otherwise "I broke down nothing, therefore I finished everything".
        let g = Goal::new("empty", "do the thing", 0);
        assert!(!g.complete());
        assert!(g.current().is_none());
    }

    #[test]
    fn completion_requires_every_phase() {
        let mut g = goal();
        while let Some(i) = g.start_current(9) {
            assert!(!g.complete());
            g.finish(i, "ok", 9);
        }
        assert!(g.complete());
        assert!(g.current().is_none());
    }

    #[test]
    fn unfinished_lists_only_what_is_still_owed() {
        let (s, d) = store();
        let mut a = Goal::new("a", "first", 10).with_phases(["one"]);
        let b = Goal::new("b", "second", 20).with_phases(["one"]);
        let i = a.start_current(11).unwrap();
        a.finish(i, "done", 12);
        s.save(&a).unwrap();
        s.save(&b).unwrap();

        let left = s.unfinished();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "b");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_crafted_id_cannot_escape_the_store_directory() {
        let (s, d) = store();
        let g = Goal::new("../../etc/passwd", "nope", 0);
        s.save(&g).unwrap();
        assert!(!std::path::Path::new("/etc/passwd.json").exists());
        let files: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        assert!(!files[0].file_name().to_string_lossy().contains(".."));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_older_goal_file_without_the_newer_fields_still_loads() {
        // Goals outlive the code that wrote them; that is the point of them.
        let json = r#"{"schema":"harness.goal.v1","id":"old","objective":"ship"}"#;
        let g: Goal = serde_json::from_str(json).unwrap();
        assert_eq!(g.objective, "ship");
        assert!(g.phases.is_empty() && g.invariants.is_empty());
    }
}
