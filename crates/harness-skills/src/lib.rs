//! `harness-skills` — agentskills.io-compliant skill loader, validator, and registry.
//!
//! ## What this crate enforces
//!
//! Strict subset of <https://agentskills.io/specification>:
//!
//! - `<name>/SKILL.md` directory layout.
//! - Frontmatter fields: `name`, `description` (required) + `license`,
//!   `compatibility`, `metadata`, `allowed-tools` (optional). Unknown
//!   top-level fields are rejected.
//! - `name`: 1–64 chars, lowercase `[a-z0-9]` + hyphen, no leading/trailing
//!   hyphen, no `--`, and **must equal the parent directory name**.
//! - `description`: 1–1024 chars.
//! - `compatibility`: ≤500 chars.
//!
//! Framework extensions live in `metadata.harness.*` per DESIGN.md §6.1.

pub mod loader;
pub mod registry;
pub mod validate;

pub use loader::*;
pub use registry::*;
pub use validate::*;

use harness_core::{Resource, Skill, SkillError, SkillManifest};
use std::borrow::Cow;
use std::sync::Arc;

/// A skill loaded from disk: manifest, full body, and resource index.
#[derive(Debug, Clone)]
pub struct FileSkill {
    manifest:  SkillManifest,
    body:      String,
    resources: Vec<Resource>,
}

impl FileSkill {
    pub fn new(manifest: SkillManifest, body: String, resources: Vec<Resource>) -> Self {
        Self { manifest, body, resources }
    }
}

impl Skill for FileSkill {
    fn manifest(&self) -> &SkillManifest { &self.manifest }
    fn body(&self) -> Cow<'_, str> { Cow::Borrowed(&self.body) }
    fn resources(&self) -> &[Resource] { &self.resources }
}

/// Load a single skill from `<path>/SKILL.md`, returning an opaque trait object.
pub fn load_skill_dir(path: &std::path::Path) -> Result<Arc<dyn Skill>, SkillError> {
    let s = loader::load(path)?;
    Ok(Arc::new(s))
}
