//! In-process skill registry that merges filesystem-loaded skills with
//! `#[skill]`-macro-registered skills.

use harness_core::{Skill, SkillError, iter_macro_skills};
use std::collections::HashMap;
use std::sync::Arc;

/// A unified view of every skill known to the running process.
#[derive(Default, Clone)]
pub struct SkillRegistry {
    by_name: HashMap<String, Arc<dyn Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pull in every skill registered via the `#[skill]` proc-macro.
    pub fn with_macro_skills(mut self) -> Result<Self, SkillError> {
        for s in iter_macro_skills() {
            self.insert(s)?;
        }
        Ok(self)
    }

    /// Pull in every skill found under `<root>/<name>/SKILL.md`.
    pub fn with_filesystem_root(mut self, root: &std::path::Path) -> Result<Self, SkillError> {
        for s in crate::loader::scan_skills_root(root)? {
            self.insert(Arc::new(s))?;
        }
        Ok(self)
    }

    pub fn insert(&mut self, s: Arc<dyn Skill>) -> Result<(), SkillError> {
        let name = s.manifest().name.clone();
        if self.by_name.contains_key(&name) {
            return Err(SkillError::Duplicate { name });
        }
        self.by_name.insert(name, s);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Skill>> {
        self.by_name.get(name).cloned()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Skill>)> {
        self.by_name.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Render the (name, description) catalogue used at session start
    /// (progressive disclosure tier 1, ≈100 tokens per skill).
    pub fn catalogue(&self) -> String {
        let mut out = String::from("Available skills:\n");
        let mut entries: Vec<_> = self.by_name.values().collect();
        entries.sort_by(|a, b| a.manifest().name.cmp(&b.manifest().name));
        for s in entries {
            out.push_str(&format!(
                "- {}: {}\n",
                s.manifest().name,
                s.manifest().description
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Resource, Skill, SkillManifest};
    use std::borrow::Cow;

    struct Dummy(SkillManifest);
    impl Skill for Dummy {
        fn manifest(&self) -> &SkillManifest {
            &self.0
        }
        fn body(&self) -> Cow<'_, str> {
            Cow::Borrowed("")
        }
        fn resources(&self) -> &[Resource] {
            &[]
        }
    }

    fn dummy(name: &str, desc: &str) -> Arc<dyn Skill> {
        Arc::new(Dummy(SkillManifest {
            name: name.into(),
            description: desc.into(),
            license: None,
            compatibility: None,
            metadata: Default::default(),
            allowed_tools: None,
        }))
    }

    #[test]
    fn rejects_duplicate_names() {
        let mut r = SkillRegistry::new();
        r.insert(dummy("a", "first")).unwrap();
        assert!(matches!(
            r.insert(dummy("a", "second")),
            Err(SkillError::Duplicate { .. })
        ));
    }

    #[test]
    fn catalogue_is_alphabetical() {
        let mut r = SkillRegistry::new();
        r.insert(dummy("b-skill", "second")).unwrap();
        r.insert(dummy("a-skill", "first")).unwrap();
        let cat = r.catalogue();
        let a = cat.find("a-skill").unwrap();
        let b = cat.find("b-skill").unwrap();
        assert!(a < b, "a-skill should come before b-skill");
    }
}
