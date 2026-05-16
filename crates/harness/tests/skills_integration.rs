//! End-to-end test for the public surface of the harness facade:
//!
//! 1. `#[skill]`-decorated Rust functions auto-register via `inventory`.
//! 2. SKILL.md directories load and validate against agentskills.io.
//! 3. `SkillRegistry` merges both sources.
//! 4. Malformed SKILL.md is rejected with a precise error.

use harness::prelude::*;
use harness::skills;
use std::path::PathBuf;

/// Echo the user's input. Use when the user asks the agent to repeat something verbatim.
#[harness::skill(
    name = "echo",
    license = "MIT",
    harness(kind = "computational", risk = "read-only")
)]
async fn echo(_ctx: &mut Context, _world: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

/// Format a Rust workspace using cargo fmt. Use before committing Rust code.
#[harness::skill(name = "format-rust")]
async fn format_rust(_ctx: &mut Context, _world: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")
}

#[test]
fn macro_skills_register_via_inventory() {
    let registry = skills::SkillRegistry::new()
        .with_macro_skills()
        .expect("macro skills register cleanly");

    let echo = registry.get("echo").expect("echo registered");
    let manifest = echo.manifest();
    assert_eq!(manifest.name, "echo");
    assert!(manifest.description.starts_with("Echo the user's input."));
    assert_eq!(manifest.license.as_deref(), Some("MIT"));

    // Extension lives in metadata.harness.*
    let ext = manifest.harness_ext().expect("harness ext present");
    assert_eq!(ext.kind, Some(harness::Execution::Computational));
    assert_eq!(ext.risk, Some(harness::ToolRisk::ReadOnly));

    // Description-from-doc-comment works.
    let fmt = registry.get("format-rust").expect("format-rust registered");
    assert!(
        fmt.manifest()
            .description
            .starts_with("Format a Rust workspace using cargo fmt.")
    );

    // Handler is present for both.
    assert!(echo.handler().is_some());
    assert!(fmt.handler().is_some());
}

#[test]
fn filesystem_skill_loads_via_spec() {
    let root = fixtures_root();
    let hello = skills::load_skill_dir(&root.join("hello-world")).expect("valid skill loads");
    let m = hello.manifest();
    assert_eq!(m.name, "hello-world");
    assert!(m.description.contains("Greet a user by name"));
    assert_eq!(m.license.as_deref(), Some("Apache-2.0"));

    let ext = m.harness_ext().expect("harness ext present");
    assert_eq!(ext.kind, Some(harness::Execution::Inferential));

    // Body content survives parsing.
    assert!(hello.body().contains("# Hello World"));
}

#[test]
fn broken_skill_is_rejected() {
    let root = fixtures_root();
    let err = match skills::load_skill_dir(&root.join("broken")) {
        Ok(_) => panic!("broken skill should fail validation"),
        Err(e) => e,
    };
    // Name regex violation OR dir mismatch — either is acceptable; spec
    // says name must match dir AND must be valid. We assert the message is
    // informative.
    let msg = err.to_string();
    assert!(
        msg.contains("BROKEN-NAME") || msg.to_lowercase().contains("name"),
        "expected name-related error, got: {msg}"
    );
}

#[test]
fn registry_merges_filesystem_and_macro_skills() {
    let valid_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills_valid");
    let registry = skills::SkillRegistry::new()
        .with_macro_skills()
        .unwrap()
        .with_filesystem_root(&valid_root)
        .unwrap_or_else(|e| panic!("filesystem merge failed: {e}"));

    assert!(registry.get("echo").is_some());
    assert!(registry.get("hello-world").is_some());
    assert!(
        registry.len() >= 3,
        "expected ≥3 skills, got {}",
        registry.len()
    );

    // Catalogue is alphabetical and includes both sources.
    let cat = registry.catalogue();
    let pos_echo = cat.find("\n- echo:").expect("echo in catalogue");
    let pos_fmt = cat
        .find("\n- format-rust:")
        .expect("format-rust in catalogue");
    let pos_hello = cat
        .find("\n- hello-world:")
        .expect("hello-world in catalogue");
    assert!(pos_echo < pos_fmt);
    assert!(pos_fmt < pos_hello);
}

/// audit #11: end-to-end round-trip for `#[skill]`-macro-registered skills.
///
/// 1. Build a registry from `inventory` (no filesystem skills)
/// 2. Export it to a temp dir
/// 3. Validate every emitted SKILL.md against the agentskills.io spec
/// 4. Re-load the export tree, confirm everything round-trips
/// 5. Confirm the macro-emitted skill carries `metadata.harness.{kind,risk}`
///    in the published SKILL.md (so external agents see the framework hints)
#[test]
fn macro_skill_round_trips_via_export() {
    let registry = skills::SkillRegistry::new()
        .with_macro_skills()
        .expect("macro skills register");

    let tmp = std::env::temp_dir().join(format!(
        "harness-export-roundtrip-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();

    // 2. Export everything
    let written = skills::export_registry(&registry, &tmp).expect("export_registry succeeds");
    assert!(!written.is_empty(), "at least one skill exported");

    // 3. Each emitted SKILL.md must validate
    for path in &written {
        assert!(path.exists(), "exported path exists: {}", path.display());
        let body = std::fs::read_to_string(path).unwrap();
        assert!(
            body.starts_with("---\n"),
            "SKILL.md starts with YAML frontmatter"
        );
        assert!(body.contains("---\n\n"), "frontmatter is terminated");
    }

    // 4. Reload — does the registry see every skill we exported?
    let reloaded = skills::SkillRegistry::new()
        .with_filesystem_root(&tmp)
        .expect("reload from exported tree");
    assert_eq!(reloaded.len(), written.len());
    let echo_reloaded = reloaded.get("echo").expect("echo round-trips");
    assert_eq!(echo_reloaded.manifest().name, "echo");
    assert_eq!(echo_reloaded.manifest().license.as_deref(), Some("MIT"));

    // 5. metadata.harness.* preserved
    let harness_ext = echo_reloaded
        .manifest()
        .harness_ext()
        .expect("harness ext preserved");
    assert!(matches!(
        harness_ext.kind,
        Some(harness::Execution::Computational)
    ));

    let _ = std::fs::remove_dir_all(&tmp);
}
