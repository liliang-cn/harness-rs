//! Programmatic skill writing — create/overwrite/delete a `<dir>/<name>/SKILL.md`
//! with validate-on-write. Used by the `skill_manage` tool (learning loop) so an
//! agent can author skills at runtime. Read paths live in `loader`/`registry`.

use harness_core::{Skill, SkillError};
use std::path::{Path, PathBuf};

/// Write a full SKILL.md (`content` = frontmatter + body) to `<dir>/<name>/SKILL.md`.
///
/// Validates the result by loading it back; if invalid, the write is rolled back
/// (prior content restored, or the new skill dir removed) and the error returned —
/// no half-written invalid skill survives. `name` must pass agentskills.io rules.
pub fn write_skill_md(dir: &Path, name: &str, content: &str) -> Result<PathBuf, SkillError> {
    crate::validate::validate_name(name)?;
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).map_err(|e| SkillError::Io(e.to_string()))?;
    let path = skill_dir.join("SKILL.md");
    let prior = std::fs::read(&path).ok();
    std::fs::write(&path, content).map_err(|e| SkillError::Io(e.to_string()))?;
    // Validate by loading it back.
    let loaded_ok = crate::loader::load(&skill_dir)
        .and_then(|fs| crate::validate::validate(fs.manifest()).map(|_| ()));
    match loaded_ok {
        Ok(()) => Ok(path),
        Err(e) => {
            match prior {
                Some(bytes) => { let _ = std::fs::write(&path, bytes); }
                None => { let _ = std::fs::remove_dir_all(&skill_dir); }
            }
            Err(e)
        }
    }
}

/// Remove `<dir>/<name>/` entirely. Returns `true` if it existed.
pub fn delete_skill(dir: &Path, name: &str) -> Result<bool, SkillError> {
    crate::validate::validate_name(name)?;
    let skill_dir = dir.join(name);
    if skill_dir.exists() {
        std::fs::remove_dir_all(&skill_dir).map_err(|e| SkillError::Io(e.to_string()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-skillwrite-{}-{n}", std::process::id()))
    }

    const VALID: &str = "---\nname: deploy-runbook\ndescription: How to deploy the service.\n---\n# Deploy\n1. build\n2. ship\n";

    #[test]
    fn write_then_load_roundtrips() {
        let dir = tmp();
        let p = write_skill_md(&dir, "deploy-runbook", VALID).unwrap();
        assert!(p.exists());
        let loaded = crate::loader::load(&dir.join("deploy-runbook")).unwrap();
        assert_eq!(loaded.manifest().name, "deploy-runbook");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_content_rolls_back_and_errors() {
        let dir = tmp();
        let bad = "no frontmatter here";
        let err = write_skill_md(&dir, "broken", bad);
        assert!(err.is_err(), "invalid skill must error");
        assert!(!dir.join("broken").join("SKILL.md").exists(), "no file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_name_rejected() {
        let dir = tmp();
        assert!(write_skill_md(&dir, "Bad Name!", VALID).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_dir() {
        let dir = tmp();
        write_skill_md(&dir, "deploy-runbook", VALID).unwrap();
        assert!(delete_skill(&dir, "deploy-runbook").unwrap());
        assert!(!delete_skill(&dir, "deploy-runbook").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
