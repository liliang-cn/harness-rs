//! `Skill` trait — strictly aligned with the
//! [agentskills.io](https://agentskills.io/specification) specification.
//!
//! See DESIGN.md §6 for how this maps onto the public spec and which fields
//! are framework extensions (anything under `metadata.harness.*`).

use crate::{Context, World, error::SkillError};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The frontmatter of a `SKILL.md`, exactly as specified.
///
/// All fields except `name` and `description` are optional, mirroring the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    /// Free-form key-value map. Framework extensions live under `metadata.harness.*`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// Space-separated tool patterns (e.g. "Bash(git:*) Read"). Experimental in the spec.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "allowed-tools"
    )]
    pub allowed_tools: Option<String>,
}

impl SkillManifest {
    /// Read framework-specific extensions from `metadata.harness`.
    pub fn harness_ext(&self) -> Option<HarnessExt> {
        let v = self.metadata.get("harness")?;
        serde_json::from_value::<HarnessExt>(v.clone()).ok()
    }
}

/// Optional `metadata.harness.*` sub-tree. Spec-compliant: keys other agents
/// don't recognise are simply ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessExt {
    #[serde(default)]
    pub kind: Option<crate::Execution>,
    #[serde(default)]
    pub risk: Option<crate::ToolRisk>,
    /// Rust function path; only meaningful for `#[skill]` macro-generated skills.
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub schema_version: Option<String>,
}

/// A non-`SKILL.md` resource bundled with the skill (`scripts/*`, `references/*`, `assets/*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub kind: ResourceKind,
    pub path: PathBuf,
    /// 1-line description used in progressive disclosure.
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ResourceKind {
    Script,
    Reference,
    Asset,
}

/// Optional in-process handler attached to a skill (only `#[skill]`-generated skills carry one).
pub type SkillHandler = std::sync::Arc<
    dyn for<'a> Fn(
            &'a mut Context,
            &'a mut World,
        ) -> futures::future::BoxFuture<'a, Result<(), SkillError>>
        + Send
        + Sync,
>;

pub trait Skill: Send + Sync + 'static {
    fn manifest(&self) -> &SkillManifest;
    /// The full Markdown body, loaded on demand (progressive disclosure tier 2).
    fn body(&self) -> Cow<'_, str>;
    fn resources(&self) -> &[Resource] {
        &[]
    }
    fn handler(&self) -> Option<SkillHandler> {
        None
    }
}

/// `inventory` slot for compile-time skill registration via `#[skill]`.
///
/// Macro-generated skills `inventory::submit!` a `SkillEntry` here so that
/// `harness::skills::all()` can enumerate them at runtime without any IoC container.
pub struct SkillEntry {
    pub factory: fn() -> std::sync::Arc<dyn Skill>,
}

inventory::collect!(SkillEntry);

/// Enumerate every `#[skill]`-registered skill (filesystem-loaded skills are
/// separate — see `harness-skills` for `scan_skills_root`).
pub fn iter_macro_skills() -> impl Iterator<Item = std::sync::Arc<dyn Skill>> {
    inventory::iter::<SkillEntry>().map(|e| (e.factory)())
}
