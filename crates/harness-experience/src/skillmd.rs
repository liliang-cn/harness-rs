//! SKILL.md assembly, splitting, and the text heuristics both halves of the
//! closed learning loop share.
//!
//! ## Why the model never writes the frontmatter
//!
//! The distiller and reviser ask the model for *fields* (name, description,
//! steps), never for a finished `SKILL.md`. This module then assembles the file.
//! That inversion is the whole reason a distilled draft can be guaranteed to
//! pass `harness_skills::validate` + `lint`: every spec rule (name regex, 1024-
//! char description ceiling, YAML quoting, `name == parent dir`) is enforced by
//! Rust here rather than requested politely in a prompt. A model that emits
//! `name: "Deploy Runbook (v2)"` produces a valid skill anyway, because
//! [`slugify`] runs before anything touches the disk.
//!
//! Serialization goes through [`SkillManifest`] and `serde_yaml` rather than
//! `format!`, so a description containing `:`, `#`, or a quote can't produce a
//! file that parses as something else.

use harness_core::{SkillError, SkillManifest};
use std::collections::BTreeMap;

/// Longest name the spec allows.
const NAME_MAX: usize = 64;
/// Longest description the spec allows.
const DESCRIPTION_MAX: usize = 1024;
/// Shortest description that clears `harness_skills::lint`'s R1 (≥30) *and* R4
/// (a description under 60 chars that restates the name is flagged as low
/// signal). Targeting 60 sidesteps both with one rule.
const DESCRIPTION_MIN: usize = 60;

/// Routing cues `harness_skills::lint` R2 accepts. That list lives as a
/// function-local literal inside `lint_skills` and is not exported, so a subset
/// of the most natural phrasings is mirrored here. Only used to decide whether
/// we must *append* a cue — a false negative costs one redundant "Use when…"
/// clause, never an invalid skill.
const ROUTING_CUES: &[&str] = &[
    "use when",
    "use for",
    "use this skill",
    "use after",
    "use before",
    "when the user",
    "whenever the user",
    "trigger when",
    "trigger whenever",
    "invoke when",
    "applies when",
];

/// English tokens too common to carry topical signal. Deliberately tiny: the
/// `len >= 4` filter already removes most function words, and an aggressive
/// stoplist would make two unrelated skills look alike.
const STOPWORDS: &[&str] = &[
    "that", "this", "with", "from", "into", "then", "than", "them", "they", "have", "been", "were",
    "will", "would", "should", "could", "when", "what", "which", "while", "after", "before",
    "about", "there", "their", "your", "user", "asks", "asked", "skill", "step", "steps", "using",
    "used", "make", "makes", "does", "done",
];

/// Reduce free text to an agentskills.io-legal name: lowercase `[a-z0-9]`
/// separated by single hyphens, ≤64 chars, no leading/trailing hyphen.
///
/// Returns an empty string when nothing survives (e.g. a CJK-only name); the
/// caller substitutes a fallback rather than writing an invalid skill.
pub(crate) fn slugify(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_hyphen = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }
    // Truncate on a hyphen boundary so we never end mid-word with a dangling
    // fragment like `deploy-produc`.
    if out.len() > NAME_MAX {
        out.truncate(NAME_MAX);
        if let Some(i) = out.rfind('-') {
            out.truncate(i);
        }
    }
    out.trim_matches('-').to_string()
}

/// Collapse a model-authored description onto one whitespace-normalised line
/// within the spec's 1024-char ceiling.
///
/// One line matters: YAML would happily fold a multi-line scalar, but the
/// catalogue renders `- {name}: {description}` per line, and a description with
/// an embedded newline silently breaks that alignment for every downstream
/// reader.
pub(crate) fn sanitize_description(raw: &str) -> String {
    let mut s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > DESCRIPTION_MAX {
        s.truncate(floor_char_boundary(&s, DESCRIPTION_MAX - 1));
        s.push('…');
    }
    s
}

/// Make a description lint-clean: whitespace-normalised, carrying a routing
/// cue, and at least [`DESCRIPTION_MIN`] chars, using `situation` as the source
/// of the missing "when".
pub(crate) fn finalize_description(raw: &str, situation: &str, fallback: &str) -> String {
    let mut s = sanitize_description(raw);
    if s.is_empty() {
        s = sanitize_description(fallback);
    }
    let situation = sanitize_description(situation);
    if !ROUTING_CUES
        .iter()
        .any(|c| s.to_ascii_lowercase().contains(c))
    {
        let when = first_sentence(&situation, 160);
        if !when.is_empty() {
            s = format!("{} Use when {}.", end_sentence(&s), lower_first(&when));
        }
    }
    // Still too thin for lint R1/R4 — say where it came from rather than pad
    // with filler, so a human reviewing the skill learns something.
    if s.len() < DESCRIPTION_MIN && !situation.is_empty() {
        s = format!(
            "{} Distilled from a past run: {}.",
            end_sentence(&s),
            first_sentence(&situation, 200)
        );
    }
    sanitize_description(&s)
}

/// Terminate `s` with a full stop unless it already ends in sentence
/// punctuation, so appended clauses don't run into the previous sentence.
fn end_sentence(s: &str) -> String {
    let t = s.trim_end();
    if t.is_empty() || t.ends_with(['.', '!', '?', ':', ';', '…']) {
        t.to_string()
    } else {
        format!("{t}.")
    }
}

/// First sentence of `text`, hard-capped at `max` chars.
fn first_sentence(text: &str, max: usize) -> String {
    let end = text.find(['.', '!', '?', '\n']).map_or(text.len(), |i| i);
    let mut s = text[..end].trim().to_string();
    if s.len() > max {
        s.truncate(floor_char_boundary(&s, max));
        s = s.trim_end().to_string();
    }
    s
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // Only lowercase a leading ASCII capital: downcasing an acronym's first
        // letter ("HTTP" → "hTTP") reads worse than leaving it alone.
        Some(f) if f.is_ascii_uppercase() && !s.chars().take(2).all(|c| c.is_ascii_uppercase()) => {
            f.to_ascii_lowercase().to_string() + c.as_str()
        }
        _ => s.to_string(),
    }
}

/// Largest index ≤ `i` that lands on a UTF-8 char boundary.
/// (`str::floor_char_boundary` is still unstable.)
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Build a `SkillManifest` with the fields this crate ever sets.
pub(crate) fn manifest(
    name: &str,
    description: &str,
    metadata: BTreeMap<String, serde_json::Value>,
) -> SkillManifest {
    SkillManifest {
        name: name.to_string(),
        description: description.to_string(),
        license: None,
        compatibility: None,
        metadata,
        allowed_tools: None,
    }
}

/// Render `---\n<yaml>---\n<body>\n`, the exact shape
/// `harness_skills::write_skill_md` round-trips.
pub(crate) fn render_skill_md(m: &SkillManifest, body: &str) -> String {
    // Infallible in practice — SkillManifest is a plain struct of strings and a
    // JSON map — but a serializer error must not panic a server, so degrade to
    // the two required fields rather than unwrap.
    let yaml = serde_yaml::to_string(m).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "skill frontmatter serialization failed; emitting minimal frontmatter");
        format!("name: {}\ndescription: {}\n", m.name, m.description)
    });
    // serde_yaml 0.9 does not emit a document-start marker, but strip one
    // defensively: a stray `---` would terminate the frontmatter early.
    let yaml = yaml.strip_prefix("---\n").unwrap_or(&yaml);
    let yaml = if yaml.ends_with('\n') {
        yaml.to_string()
    } else {
        format!("{yaml}\n")
    };
    format!("---\n{yaml}---\n{}\n", body.trim_end())
}

/// Split a SKILL.md into manifest + body. Mirrors `harness_skills::loader`'s
/// private frontmatter parser; reimplemented rather than exported-from-there so
/// this crate's revision path doesn't force an API change on `harness-skills`
/// (three other agents are in this workspace).
pub(crate) fn split_skill_md(raw: &str) -> Result<(SkillManifest, String), SkillError> {
    if !raw.starts_with("---") {
        return Err(SkillError::Invalid {
            path: "<in-memory SKILL.md>".into(),
            reason: "missing leading `---` frontmatter delimiter".into(),
        });
    }
    let rest = &raw[3..];
    let end = rest.find("\n---").ok_or_else(|| SkillError::Invalid {
        path: "<in-memory SKILL.md>".into(),
        reason: "missing closing `---` for frontmatter".into(),
    })?;
    let yaml_str = &rest[..end];
    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after);
    let m: SkillManifest = serde_yaml::from_str(yaml_str).map_err(|e| SkillError::Invalid {
        path: "<in-memory SKILL.md>".into(),
        reason: format!("YAML schema: {e}"),
    })?;
    Ok((m, body.to_string()))
}

/// Run the crate's own validator + linter over a candidate skill, returning
/// every Error/Warning finding as a human-readable string.
///
/// Info findings are tolerated (they're style nudges; `harness_skills`' own
/// "good skill passes clean" test tolerates them too). Cross-skill lint rules
/// can't fire on a one-element slice, so the only reachable rules are the
/// per-skill R1–R4 — the ones [`finalize_description`] and body assembly are
/// built to satisfy.
pub(crate) fn validate_and_lint(m: &SkillManifest, body: &str) -> Result<(), Vec<String>> {
    if let Err(e) = harness_skills::validate(m) {
        return Err(vec![e.to_string()]);
    }
    let candidate = harness_skills::FileSkill::new(m.clone(), body.to_string(), Vec::new());
    let complaints: Vec<String> = harness_skills::lint_skills(&[candidate])
        .into_iter()
        .filter(|f| f.severity != harness_skills::LintSeverity::Info)
        .map(|f| format!("{:?}: {}", f.severity, f.message))
        .collect();
    if complaints.is_empty() {
        Ok(())
    } else {
        Err(complaints)
    }
}

// ---------------------------------------------------------------------------
// Similarity
// ---------------------------------------------------------------------------

/// Topical tokens of a text: lowercased alphanumeric words of ≥4 chars, minus
/// [`STOPWORDS`].
fn tokens(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Overlap coefficient (Szymkiewicz–Simpson): `|A ∩ B| / min(|A|, |B|)`, in
/// `0.0..=1.0`.
///
/// **Not Jaccard**, and the difference decides whether duplicate suppression
/// works at all. The two texts compared are structurally lopsided: an episode's
/// `situation` is one sentence ("deploy the website to production"), while a
/// skill `description` is deliberately padded with routing cues ("…Use when the
/// user asks to ship, deploy, or release the site."). Jaccard divides by the
/// union, so the longer description's extra tokens push a genuine duplicate down
/// to ~0.3 — below any threshold that isn't also matching unrelated pairs.
/// Dividing by the *smaller* set asks the question we actually mean: "is the
/// short text essentially contained in the long one?"
pub(crate) fn overlap_similarity(a: &str, b: &str) -> f32 {
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count();
    inter as f32 / ta.len().min(tb.len()) as f32
}

// ---------------------------------------------------------------------------
// Dates (for the revision cap)
// ---------------------------------------------------------------------------

/// Today's UTC date as `YYYY-MM-DD`.
///
/// Hand-rolled rather than pulling `chrono` into an optional crate: the revision
/// cap needs exactly one thing from a calendar — "is this the same day as last
/// time?" — and a string produced once per revision is cheaper than a dependency.
pub(crate) fn utc_today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`: days-since-1970-01-01 → (year, month,
/// day) in the proleptic Gregorian calendar. Exact for every value we can hit.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_produces_spec_legal_names() {
        assert_eq!(slugify("Deploy Runbook (v2)"), "deploy-runbook-v2");
        assert_eq!(slugify("  --Fix__Bug-- "), "fix-bug");
        assert_eq!(slugify("PDF Processing"), "pdf-processing");
        assert_eq!(
            slugify("你好"),
            "",
            "no ASCII survivors → caller falls back"
        );
        for input in [
            "Deploy Runbook (v2)",
            "a".repeat(200).as_str(),
            "one two three four five six seven eight nine ten eleven twelve thirteen",
        ] {
            let s = slugify(input);
            assert!(
                harness_skills::validate_name(&s).is_ok(),
                "slugify({input:?}) = {s:?} must be a legal skill name"
            );
        }
    }

    #[test]
    fn finalize_description_adds_a_routing_cue_and_length() {
        let d = finalize_description(
            "Deploys the site.",
            "the user asked to deploy the marketing site to production",
            "fallback",
        );
        assert!(d.to_ascii_lowercase().contains("use when"), "{d}");
        assert!(d.len() >= DESCRIPTION_MIN, "{} chars: {d}", d.len());
        assert!(!d.contains('\n'));
    }

    #[test]
    fn finalize_description_keeps_an_existing_cue() {
        let original = "Deploy the marketing site to production. Use when the user asks to ship or release the site.";
        assert_eq!(finalize_description(original, "whatever", "f"), original);
    }

    #[test]
    fn render_and_split_round_trip_with_awkward_text() {
        // A description full of YAML metacharacters must survive verbatim.
        let desc = "Deploy: the site — see #ops, \"carefully\", 100% of the time. Use when asked.";
        let mut meta = BTreeMap::new();
        meta.insert("experience".into(), serde_json::json!({"revision": 3}));
        let m = manifest("deploy-runbook", desc, meta);
        let md = render_skill_md(&m, "# Deploy\n\n1. build\n");
        let (back, body) = split_skill_md(&md).unwrap();
        assert_eq!(back.name, "deploy-runbook");
        assert_eq!(back.description, desc);
        assert_eq!(back.metadata["experience"]["revision"], 3);
        assert!(body.contains("1. build"));
    }

    #[test]
    fn overlap_beats_jaccard_on_a_lopsided_pair() {
        let situation = "deploy the website to production and verify it is live";
        let description = "Deploy the website to production. Use when the user asks to ship, \
                           release, or push the site live.";
        let sim = overlap_similarity(situation, description);
        assert!(sim >= 0.6, "genuine duplicate scored only {sim}");
        let unrelated = "Summarise a PDF invoice into a spreadsheet row.";
        assert!(overlap_similarity(situation, unrelated) < 0.3);
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // Leap day, and the day after.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }

    #[test]
    fn utc_today_is_a_plausible_iso_date() {
        let t = utc_today();
        assert_eq!(t.len(), 10, "{t}");
        assert!(t.starts_with("20"), "{t}");
        assert_eq!(t.matches('-').count(), 2, "{t}");
    }
}
