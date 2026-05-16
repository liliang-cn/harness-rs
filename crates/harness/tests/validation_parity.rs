//! Drift guard: the skill-name validator runs in two places —
//! `harness-macros` (compile-time, on `#[skill]`) and `harness-skills`
//! (runtime, on filesystem `SKILL.md` load). They must agree on every input.
//!
//! This test exercises a curated corpus so any future divergence trips a
//! red CI immediately.

use harness_skills::validate::validate_name;

/// Mirror of `harness-macros::validate_skill_name` (which can't be imported
/// directly because it's a proc-macro crate). If you edit the macro's
/// validator, update this mirror too.
fn macro_validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.len() > 64 {
        return Err(format!("name length {} > 64", name.len()));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("name must not start or end with `-`".into());
    }
    if name.contains("--") {
        return Err("name must not contain `--`".into());
    }
    for (i, c) in name.char_indices() {
        if !(c.is_ascii_digit() || ('a'..='z').contains(&c) || c == '-') {
            return Err(format!("name contains invalid char `{c}` at byte {i}"));
        }
    }
    Ok(())
}

#[test]
fn validators_agree_on_corpus() {
    let cases: &[&str] = &[
        // valid
        "format-rust",
        "data-analysis",
        "code-review",
        "a",
        "a1",
        "a-b-c-d-e",
        "x9",
        // invalid
        "",
        "PDF-Processing",
        "-pdf",
        "pdf-",
        "pdf--processing",
        "pdf_x",
        "pdf.x",
        "PDF",
        " pdf",
        "pdf ",
    ];
    for name in cases {
        let runtime = validate_name(name).is_ok();
        let macro_  = macro_validate_name(name).is_ok();
        assert_eq!(
            runtime, macro_,
            "validator disagreement for {name:?}: runtime={runtime}, macro={macro_}"
        );
    }

    // Length boundary
    let max_ok = "a".repeat(64);
    let too_long = "a".repeat(65);
    assert!(validate_name(&max_ok).is_ok());
    assert!(macro_validate_name(&max_ok).is_ok());
    assert!(validate_name(&too_long).is_err());
    assert!(macro_validate_name(&too_long).is_err());
}
