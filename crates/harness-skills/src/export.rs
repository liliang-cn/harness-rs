//! Export every registered skill as a spec-compliant SKILL.md directory.
//!
//! Output layout (per agentskills.io):
//!
//! ```text
//! <target>/
//!   <name>/
//!     SKILL.md     # frontmatter + body
//! ```
//!
//! Exported directories can be consumed by Claude Code, Cursor, Codex, or any
//! other agent that follows the spec — this is the framework's portability
//! contract from DESIGN.md §6.10.

use harness_core::{Skill, SkillError, SkillManifest};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Write a single skill out as `<target>/<name>/SKILL.md`.
pub fn export_one(skill: &Arc<dyn Skill>, target: &Path) -> Result<std::path::PathBuf, SkillError> {
    let manifest = skill.manifest();
    crate::validate::validate(manifest)?;
    let dir = target.join(&manifest.name);
    std::fs::create_dir_all(&dir).map_err(|e| SkillError::Io(e.to_string()))?;
    let md = render_skill_md(manifest, &skill.body());
    let path = dir.join("SKILL.md");
    std::fs::write(&path, md).map_err(|e| SkillError::Io(e.to_string()))?;
    Ok(path)
}

/// Export every skill in a registry. Returns paths written.
pub fn export_registry(
    registry: &crate::SkillRegistry,
    target: &Path,
) -> Result<Vec<std::path::PathBuf>, SkillError> {
    std::fs::create_dir_all(target).map_err(|e| SkillError::Io(e.to_string()))?;
    let mut paths = Vec::new();
    for (_, skill) in registry.iter() {
        paths.push(export_one(skill, target)?);
    }
    Ok(paths)
}

/// Render a `SKILL.md` string from a manifest + body.
///
/// Key ordering of YAML frontmatter matches the spec's documentation order.
pub fn render_skill_md(m: &SkillManifest, body: &str) -> String {
    // Build a manual YAML so field order is stable, deterministic, and matches the spec.
    let mut yaml = String::new();
    yaml.push_str("---\n");
    yaml.push_str(&format!("name: {}\n", m.name));
    yaml.push_str(&format!("description: {}\n", yaml_inline(&m.description)));
    if let Some(l) = &m.license {
        yaml.push_str(&format!("license: {l}\n"));
    }
    if let Some(c) = &m.compatibility {
        yaml.push_str(&format!("compatibility: {}\n", yaml_inline(c)));
    }
    if let Some(a) = &m.allowed_tools {
        yaml.push_str(&format!("allowed-tools: {a}\n"));
    }
    if !m.metadata.is_empty() {
        yaml.push_str("metadata:\n");
        emit_yaml_map(&mut yaml, &m.metadata, 1);
    }
    yaml.push_str("---\n");
    yaml.push('\n');
    yaml.push_str(body);
    if !body.ends_with('\n') {
        yaml.push('\n');
    }
    yaml
}

fn yaml_inline(s: &str) -> String {
    // Quote when needed for YAML safety.
    let needs_quote = s.contains(':') || s.contains('#') || s.contains('\n') || s.starts_with(' ');
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn emit_yaml_map(out: &mut String, m: &BTreeMap<String, serde_json::Value>, indent: usize) {
    let pad = "  ".repeat(indent);
    for (k, v) in m {
        match v {
            serde_json::Value::Object(o) => {
                out.push_str(&format!("{pad}{k}:\n"));
                let sub: BTreeMap<_, _> = o.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                emit_yaml_map(out, &sub, indent + 1);
            }
            serde_json::Value::String(s) => {
                out.push_str(&format!("{pad}{k}: {}\n", yaml_inline(s)));
            }
            other => {
                out.push_str(&format!("{pad}{k}: {other}\n"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Skill;
    use std::borrow::Cow;

    struct DummySkill(SkillManifest, String);
    impl Skill for DummySkill {
        fn manifest(&self) -> &SkillManifest {
            &self.0
        }
        fn body(&self) -> Cow<'_, str> {
            Cow::Borrowed(&self.1)
        }
    }

    #[test]
    fn render_skill_md_has_spec_layout() {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            "harness".into(),
            serde_json::json!({"kind": "computational", "risk": "read-only"}),
        );
        let m = SkillManifest {
            name: "format-rust".into(),
            description: "Run cargo fmt across the workspace.".into(),
            license: Some("Apache-2.0".into()),
            compatibility: None,
            metadata,
            allowed_tools: Some("Bash(cargo:fmt)".into()),
        };
        let s = render_skill_md(&m, "# Body\nGo brrr.\n");
        assert!(s.starts_with("---\nname: format-rust\n"));
        assert!(s.contains("description: Run cargo fmt"));
        assert!(s.contains("license: Apache-2.0\n"));
        assert!(s.contains("allowed-tools: Bash(cargo:fmt)\n"));
        assert!(s.contains("metadata:\n"));
        assert!(s.contains("  harness:\n"));
        assert!(s.contains("    kind: computational\n"));
        assert!(s.contains("    risk: read-only\n"));
        assert!(s.trim_end().ends_with("Go brrr."));
    }

    #[test]
    fn export_one_writes_valid_skill_md() {
        let tmp = std::env::temp_dir().join(format!(
            "harness-export-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&tmp);

        let manifest = SkillManifest {
            name: "x".into(),
            description: "demo".into(),
            license: None,
            compatibility: None,
            metadata: Default::default(),
            allowed_tools: None,
        };
        let skill: Arc<dyn Skill> = Arc::new(DummySkill(manifest, "body\n".into()));
        let path = export_one(&skill, &tmp).unwrap();
        assert!(path.exists());
        // And round-trip: loading the exported dir works.
        let reloaded = crate::loader::load(&tmp.join("x")).unwrap();
        assert_eq!(reloaded.manifest().name, "x");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
