//! Filesystem loader for skill directories.

use crate::{FileSkill, validate::validate_against_dir};
use harness_core::{Resource, ResourceKind, SkillError, SkillManifest};
use std::path::Path;
use walkdir::WalkDir;

/// Load a single skill from `<path>/SKILL.md`.
pub fn load(path: &Path) -> Result<FileSkill, SkillError> {
    let skill_md = path.join("SKILL.md");
    let raw = std::fs::read_to_string(&skill_md)
        .map_err(|e| SkillError::Io(format!("{}: {e}", skill_md.display())))?;

    let (manifest, body) = parse_frontmatter(&raw, &skill_md)?;

    // dir name must equal manifest.name per spec
    let dir_name =
        path.file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| SkillError::Invalid {
                path: path.display().to_string(),
                reason: "directory has no valid name".into(),
            })?;
    validate_against_dir(&manifest, dir_name)?;

    let resources = scan_resources(path);

    Ok(FileSkill::new(manifest, body.to_string(), resources))
}

/// Scan a root for `<name>/SKILL.md`. Returns one entry per valid skill.
/// Invalid skills produce a per-entry error so the caller can surface them in
/// `harness skills validate --all` mode without aborting.
pub fn scan_skills_root(root: &Path) -> Result<Vec<FileSkill>, SkillError> {
    let mut skills = Vec::new();
    if !root.exists() {
        return Err(SkillError::Io(format!("not found: {}", root.display())));
    }
    for entry in
        std::fs::read_dir(root).map_err(|e| SkillError::Io(format!("{}: {e}", root.display())))?
    {
        let entry = entry.map_err(|e| SkillError::Io(e.to_string()))?;
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if !p.join("SKILL.md").is_file() {
            continue;
        }
        skills.push(load(&p)?);
    }
    Ok(skills)
}

fn parse_frontmatter<'a>(raw: &'a str, p: &Path) -> Result<(SkillManifest, &'a str), SkillError> {
    let bytes = raw.as_bytes();
    if !raw.starts_with("---") {
        return Err(SkillError::Invalid {
            path: p.display().to_string(),
            reason: "missing leading `---` frontmatter delimiter".into(),
        });
    }
    // find the closing `\n---` after position 3
    let rest = &raw[3..];
    let end = rest.find("\n---").ok_or_else(|| SkillError::Invalid {
        path: p.display().to_string(),
        reason: "missing closing `---` for frontmatter".into(),
    })?;
    let yaml_str = &rest[..end];
    // body starts after `\n---` then optional `\n`
    let after_close = &rest[end + 4..];
    let body = after_close.strip_prefix('\n').unwrap_or(after_close);

    // First parse into a raw map so we can reject unknown top-level fields.
    let yaml_val: serde_yaml::Value =
        serde_yaml::from_str(yaml_str).map_err(|e| SkillError::Invalid {
            path: p.display().to_string(),
            reason: format!("YAML parse: {e}"),
        })?;
    reject_unknown_top_fields(&yaml_val, p)?;

    let manifest: SkillManifest =
        serde_yaml::from_value(yaml_val).map_err(|e| SkillError::Invalid {
            path: p.display().to_string(),
            reason: format!("YAML schema: {e}"),
        })?;

    // bytes & raw used to anchor the slice lifetime
    debug_assert!(body.as_ptr() >= bytes.as_ptr());
    Ok((manifest, body))
}

const KNOWN_FIELDS: &[&str] = &[
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
];

fn reject_unknown_top_fields(v: &serde_yaml::Value, p: &Path) -> Result<(), SkillError> {
    let map = match v {
        serde_yaml::Value::Mapping(m) => m,
        _ => {
            return Err(SkillError::Invalid {
                path: p.display().to_string(),
                reason: "frontmatter must be a YAML mapping".into(),
            });
        }
    };
    for (k, _) in map {
        let key = k.as_str().unwrap_or_default();
        if !KNOWN_FIELDS.contains(&key) {
            return Err(SkillError::Invalid {
                path: p.display().to_string(),
                reason: format!(
                    "unknown frontmatter field `{key}`. \
                     Spec allows only: {}. Put framework extensions under `metadata`.",
                    KNOWN_FIELDS.join(", ")
                ),
            });
        }
    }
    Ok(())
}

fn scan_resources(skill_dir: &Path) -> Vec<Resource> {
    let mut out = Vec::new();
    for (sub, kind) in [
        ("scripts", ResourceKind::Script),
        ("references", ResourceKind::Reference),
        ("assets", ResourceKind::Asset),
    ] {
        let dir = skill_dir.join(sub);
        if !dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&dir)
            .max_depth(2)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                out.push(Resource {
                    kind,
                    path: entry.path().to_path_buf(),
                    summary: first_line_summary(entry.path()),
                });
            }
        }
    }
    out
}

fn first_line_summary(p: &Path) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    s.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Skill;
    use std::fs;
    use std::path::PathBuf;
    use tempfile_workspace_dir::TestDir;

    mod tempfile_workspace_dir {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        pub struct TestDir(pub PathBuf);
        impl TestDir {
            pub fn new() -> Self {
                let pid = std::process::id();
                let n = SEQ.fetch_add(1, Ordering::SeqCst);
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let p = std::env::temp_dir().join(format!("harness-skills-test-{pid}-{nanos}-{n}"));
                std::fs::create_dir_all(&p).unwrap();
                TestDir(p)
            }
        }
        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn write_skill(root: &Path, name: &str, content: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), content).unwrap();
        dir
    }

    #[test]
    fn loads_minimal_valid_skill() {
        let td = TestDir::new();
        let p = write_skill(
            &td.0,
            "format-rust",
            "---\nname: format-rust\ndescription: Run cargo fmt across the workspace.\n---\n# body\n",
        );
        let s = load(&p).unwrap();
        assert_eq!(s.manifest().name, "format-rust");
        assert_eq!(
            s.manifest().description,
            "Run cargo fmt across the workspace."
        );
    }

    #[test]
    fn rejects_unknown_top_field() {
        let td = TestDir::new();
        let p = write_skill(
            &td.0,
            "x",
            "---\nname: x\ndescription: hi\ntriggers: [\"foo\"]\n---\nbody\n",
        );
        let err = load(&p).unwrap_err();
        match err {
            SkillError::Invalid { reason, .. } => {
                assert!(reason.contains("unknown frontmatter field `triggers`"))
            }
            e => panic!("wrong error: {e:?}"),
        }
    }

    #[test]
    fn rejects_dir_name_mismatch() {
        let td = TestDir::new();
        let p = write_skill(
            &td.0,
            "format-rust",
            "---\nname: not-format-rust\ndescription: hi\n---\n",
        );
        assert!(matches!(load(&p), Err(SkillError::NameDirMismatch { .. })));
    }

    #[test]
    fn accepts_metadata_harness_namespace() {
        let td = TestDir::new();
        let p = write_skill(
            &td.0,
            "fmt",
            "---\nname: fmt\ndescription: hi\nmetadata:\n  harness:\n    kind: computational\n    risk: read-only\n---\n",
        );
        let s = load(&p).unwrap();
        let ext = s.manifest().harness_ext().expect("harness ext present");
        assert_eq!(ext.kind, Some(harness_core::Execution::Computational));
        assert_eq!(ext.risk, Some(harness_core::ToolRisk::ReadOnly));
    }
}
