//! Strict validation against the agentskills.io specification.

use harness_core::{SkillError, SkillManifest};
use once_cell::sync::Lazy;
use regex::Regex;

/// `^[a-z0-9]+(-[a-z0-9]+)*$` — equivalent to the spec's name regex.
static NAME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-z0-9]+(-[a-z0-9]+)*$").unwrap());

const NAME_MAX: usize = 64;
const DESCRIPTION_MAX: usize = 1024;
const COMPATIBILITY_MAX: usize = 500;

/// Validate just the `name` field.
pub fn validate_name(name: &str) -> Result<(), SkillError> {
    if name.is_empty() {
        return Err(SkillError::MissingField {
            field: "name".into(),
        });
    }
    if name.len() > NAME_MAX {
        return Err(SkillError::NameRegex {
            name: name.to_string(),
            reason: format!("length {} exceeds {NAME_MAX}", name.len()),
        });
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(SkillError::NameRegex {
            name: name.to_string(),
            reason: "must not start or end with a hyphen".into(),
        });
    }
    if name.contains("--") {
        return Err(SkillError::NameRegex {
            name: name.to_string(),
            reason: "consecutive hyphens not allowed".into(),
        });
    }
    if !NAME_RE.is_match(name) {
        return Err(SkillError::NameRegex {
            name: name.to_string(),
            reason: "must contain only lowercase a-z, 0-9, and hyphens".into(),
        });
    }
    Ok(())
}

/// Validate a full manifest. Does **not** check parent-directory equality —
/// see `validate_against_dir` for that.
pub fn validate(manifest: &SkillManifest) -> Result<(), SkillError> {
    validate_name(&manifest.name)?;

    if manifest.description.is_empty() {
        return Err(SkillError::MissingField {
            field: "description".into(),
        });
    }
    if manifest.description.len() > DESCRIPTION_MAX {
        return Err(SkillError::DescriptionTooLong {
            len: manifest.description.len(),
        });
    }

    if let Some(c) = &manifest.compatibility
        && c.len() > COMPATIBILITY_MAX
    {
        return Err(SkillError::CompatibilityTooLong { len: c.len() });
    }

    Ok(())
}

/// Validate manifest and additionally check that `name == parent_dir_name`.
pub fn validate_against_dir(manifest: &SkillManifest, parent_dir: &str) -> Result<(), SkillError> {
    validate(manifest)?;
    if manifest.name != parent_dir {
        return Err(SkillError::NameDirMismatch {
            name: manifest.name.clone(),
            dir: parent_dir.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for n in [
            "pdf-processing",
            "data-analysis",
            "code-review",
            "a",
            "a1",
            "a-b-c-d",
        ] {
            assert!(validate_name(n).is_ok(), "expected `{n}` to be valid");
        }
    }

    #[test]
    fn invalid_names() {
        let long = "a".repeat(65);
        let bad = [
            "",
            "PDF-Processing",
            "-pdf",
            "pdf-",
            "pdf--processing",
            "pdf_x",
            long.as_str(),
        ];
        for n in bad {
            assert!(validate_name(n).is_err(), "expected `{n}` to be invalid");
        }
    }

    #[test]
    fn description_length_enforced() {
        let m = SkillManifest {
            name: "ok".into(),
            description: "x".repeat(DESCRIPTION_MAX + 1),
            license: None,
            compatibility: None,
            metadata: Default::default(),
            allowed_tools: None,
        };
        assert!(matches!(
            validate(&m),
            Err(SkillError::DescriptionTooLong { .. })
        ));
    }

    #[test]
    fn name_must_match_dir() {
        let m = SkillManifest {
            name: "ok".into(),
            description: "hi".into(),
            license: None,
            compatibility: None,
            metadata: Default::default(),
            allowed_tools: None,
        };
        assert!(validate_against_dir(&m, "not-ok").is_err());
        assert!(validate_against_dir(&m, "ok").is_ok());
    }
}
