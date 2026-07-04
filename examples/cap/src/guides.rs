//! Session guides: the workspace overview and the skills catalogue.

use async_trait::async_trait;
use harness_core::{
    Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, Skill, World,
};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Injects CAP's operating instructions plus a shallow workspace overview once
/// at session start.
pub struct CapGuide;
static CAP_GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static CAP_GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

#[async_trait]
impl Guide for CapGuide {
    fn id(&self) -> &GuideId {
        CAP_GUIDE_ID.get_or_init(|| "cap".into())
    }
    fn kind(&self) -> Execution {
        Execution::Inferential
    }
    fn scope(&self) -> &GuideScope {
        CAP_GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, w: &World) -> Result<(), GuideError> {
        let root = &w.repo.root;
        let mut overview = String::new();
        if let Ok(rd) = std::fs::read_dir(root) {
            let mut names: Vec<String> = rd
                .flatten()
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    !n.starts_with('.') && n != "target" && n != "node_modules"
                })
                .map(|e| {
                    let dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    format!(
                        "{}{}",
                        e.file_name().to_string_lossy(),
                        if dir { "/" } else { "" }
                    )
                })
                .collect();
            names.sort();
            overview = names.join("  ");
        }
        ctx.guides.push(Block::Text(format!(
            "You are CAP, a coding agent. You edit code with HASHLINE, not line numbers.\n\
             Workspace root: {}\n\
             Top level: {}\n\n\
             Workflow:\n\
             - `hash_read {{path}}` shows a file as `HHHH  <code>`; HHHH is a stable content anchor.\n\
             - `hash_edit` changes code by quoting anchors (replace / insert_after / insert_before / delete).\n\
             - Anchors identify a line by its content, not its position — never write anchors into files.\n\
             - `write_file` creates new files; `grep` / `glob` / `list_dir` navigate.\n\
             - Make minimal, surgical edits. hash_edit returns the refreshed view.",
            root.display(),
            if overview.is_empty() {
                "(empty)".into()
            } else {
                overview
            }
        )));
        Ok(())
    }
}

/// Injects the catalogue of available skills (name — description) once per
/// session, so the agent knows what reusable procedures exist. Bodies are
/// loaded on demand via `skill_read` (progressive disclosure).
pub struct SkillCatalog {
    pub dir: PathBuf,
}
static SKILL_GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static SKILL_GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

#[async_trait]
impl Guide for SkillCatalog {
    fn id(&self) -> &GuideId {
        SKILL_GUIDE_ID.get_or_init(|| "cap-skills".into())
    }
    fn kind(&self) -> Execution {
        Execution::Inferential
    }
    fn scope(&self) -> &GuideScope {
        SKILL_GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        let skills = harness_skills::scan_skills_root(&self.dir).unwrap_or_default();
        if skills.is_empty() {
            ctx.guides.push(Block::Text(
                "Skills: none yet. When you solve something reusable, save the procedure with \
                 `skill_manage` (create) so future runs can reuse it."
                    .into(),
            ));
        } else {
            let mut s = String::from(
                "Available skills — call `skill_read {name}` to load the steps before doing that kind of task:\n",
            );
            for sk in &skills {
                s.push_str(&format!(
                    "- {} — {}\n",
                    sk.manifest().name,
                    sk.manifest().description
                ));
            }
            ctx.guides.push(Block::Text(s));
        }
        Ok(())
    }
}
