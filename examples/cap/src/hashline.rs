//! Hashline — oh-my-pi's content-hash line anchors, reimplemented as a small,
//! dependency-free module. This is the *core* idea of CAP.
//!
//! Every line gets a short stable anchor derived from its **content**, not its
//! position. Edits reference the anchor, so:
//!
//! - Inserting at the top of a file does **not** renumber anchors below it
//!   (the anti-line-number property — the source of the token savings).
//! - Whitespace-only churn elsewhere never invalidates an unrelated patch.
//! - The model edits by quoting a 4-char anchor instead of re-emitting the
//!   whole surrounding block as an exact-match string.
//!
//! Anchors are made **unique per file**: if two lines would hash the same
//! (identical content, or a rare collision), later ones are salted until
//! distinct. Both `render` and `apply` rebuild the same anchor list from the
//! same bytes, so anchors round-trip deterministically within an unchanged file.

use std::collections::{HashMap, HashSet};

/// FNV-1a over the bytes, folded to 16 bits and printed as 4 hex chars.
fn fnv4(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{:04x}", (h ^ (h >> 16)) & 0xffff)
}

/// Compute the unique, deterministic anchor for every line of `content`.
/// Returns `(anchor, line)` pairs in file order. A blank final line (from a
/// trailing newline) is not emitted as its own anchor.
pub fn anchors(content: &str) -> Vec<(String, &str)> {
    let lines: Vec<&str> = content.split('\n').collect();
    // Drop a single trailing empty element created by a terminating '\n'.
    let effective = if lines.len() > 1 && lines.last() == Some(&"") {
        &lines[..lines.len() - 1]
    } else {
        &lines[..]
    };
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(effective.len());
    for line in effective {
        let mut anchor = fnv4(line);
        let mut salt = 0u32;
        while used.contains(&anchor) {
            salt += 1;
            anchor = fnv4(&format!("{line}\u{0}{salt}"));
        }
        used.insert(anchor.clone());
        out.push((anchor, *line));
    }
    out
}

/// Render `content` as a hashline view: `HHHH  <line>` per line. This is what
/// the agent reads; it edits by quoting the `HHHH` anchors.
pub fn render(content: &str) -> String {
    let mut s = String::new();
    for (anchor, line) in anchors(content) {
        s.push_str(&anchor);
        s.push_str("  ");
        s.push_str(line);
        s.push('\n');
    }
    s
}

/// One hashline operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Replace,
    InsertAfter,
    InsertBefore,
    Delete,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        match s {
            "replace" => Some(Op::Replace),
            "insert_after" => Some(Op::InsertAfter),
            "insert_before" => Some(Op::InsertBefore),
            "delete" => Some(Op::Delete),
            _ => None,
        }
    }
}

/// A single edit: an operation anchored at a line's content hash.
#[derive(Debug, Clone)]
pub struct Edit {
    pub op: Op,
    pub anchor: String,
    /// New text for `Replace` / `Insert*` (ignored for `Delete`). May contain
    /// its own `\n` to insert or become multiple lines.
    pub text: Option<String>,
}

/// Apply `edits` to `content`. All anchors resolve against the **original**
/// content, so a batch of edits is order-independent w.r.t. line shifting.
/// Returns the new content, or an error describing the first unresolved
/// (stale/unknown) or conflicting anchor.
pub fn apply(content: &str, edits: &[Edit]) -> Result<String, String> {
    let list = anchors(content);
    let index_of: HashMap<&str, usize> = list
        .iter()
        .enumerate()
        .map(|(i, (a, _))| (a.as_str(), i))
        .collect();

    // Per-line accumulators.
    let n = list.len();
    let mut replaced: Vec<Option<String>> = vec![None; n];
    let mut deleted = vec![false; n];
    let mut before: Vec<Vec<String>> = vec![Vec::new(); n];
    let mut after: Vec<Vec<String>> = vec![Vec::new(); n];

    for e in edits {
        let &idx = index_of.get(e.anchor.as_str()).ok_or_else(|| {
            format!(
                "stale or unknown anchor `{}` — re-run hash_read to refresh anchors",
                e.anchor
            )
        })?;
        match e.op {
            Op::Replace => {
                if deleted[idx] {
                    return Err(format!(
                        "anchor `{}`: replace conflicts with delete",
                        e.anchor
                    ));
                }
                replaced[idx] = Some(e.text.clone().unwrap_or_default());
            }
            Op::Delete => {
                if replaced[idx].is_some() {
                    return Err(format!(
                        "anchor `{}`: delete conflicts with replace",
                        e.anchor
                    ));
                }
                deleted[idx] = true;
            }
            Op::InsertBefore => before[idx].push(e.text.clone().unwrap_or_default()),
            Op::InsertAfter => after[idx].push(e.text.clone().unwrap_or_default()),
        }
    }

    let mut out: Vec<String> = Vec::with_capacity(n);
    for (i, (_, line)) in list.iter().enumerate() {
        out.extend(before[i].iter().cloned());
        if deleted[i] {
            // drop the line
        } else if let Some(new) = &replaced[i] {
            out.push(new.clone());
        } else {
            out.push((*line).to_string());
        }
        out.extend(after[i].iter().cloned());
    }

    // Preserve a trailing newline if the original had one.
    let mut result = out.join("\n");
    if content.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn main() {\n    println!(\"hi\");\n}\n";

    fn anchor_for<'a>(content: &'a str, needle: &str) -> String {
        anchors(content)
            .into_iter()
            .find(|(_, l)| l.contains(needle))
            .unwrap_or_else(|| panic!("no line containing {needle:?}"))
            .0
    }

    #[test]
    fn render_prefixes_each_line_with_a_4char_anchor() {
        let r = render(SRC);
        let first = r.lines().next().unwrap();
        assert_eq!(first.len(), 4 + 2 + "fn main() {".len());
        assert!(first.ends_with("fn main() {"));
        assert_eq!(r.lines().count(), 3); // trailing newline is not its own line
    }

    #[test]
    fn replace_one_line() {
        let a = anchor_for(SRC, "println");
        let out = apply(
            SRC,
            &[Edit {
                op: Op::Replace,
                anchor: a,
                text: Some("    println!(\"bye\");".into()),
            }],
        )
        .unwrap();
        assert!(out.contains("bye"));
        assert!(!out.contains("hi"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn insert_after_and_before() {
        let open = anchor_for(SRC, "fn main");
        let close = anchor_for(SRC, "}");
        let out = apply(
            SRC,
            &[
                Edit {
                    op: Op::InsertAfter,
                    anchor: open,
                    text: Some("    // start".into()),
                },
                Edit {
                    op: Op::InsertBefore,
                    anchor: close,
                    text: Some("    // end".into()),
                },
            ],
        )
        .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "fn main() {");
        assert_eq!(lines[1], "    // start");
        assert_eq!(lines[3], "    // end");
        assert_eq!(lines[4], "}");
    }

    #[test]
    fn delete_removes_the_line() {
        let a = anchor_for(SRC, "println");
        let out = apply(
            SRC,
            &[Edit {
                op: Op::Delete,
                anchor: a,
                text: None,
            }],
        )
        .unwrap();
        assert!(!out.contains("println"));
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn duplicate_lines_get_distinct_anchors_and_are_independently_editable() {
        let src = "x\nx\nx\n";
        let list = anchors(src);
        let uniq: HashSet<_> = list.iter().map(|(a, _)| a).collect();
        assert_eq!(
            uniq.len(),
            3,
            "three identical lines must get three anchors"
        );
        // Edit only the middle one.
        let mid = list[1].0.clone();
        let out = apply(
            src,
            &[Edit {
                op: Op::Replace,
                anchor: mid,
                text: Some("y".into()),
            }],
        )
        .unwrap();
        assert_eq!(out, "x\ny\nx\n");
    }

    #[test]
    fn anchor_is_stable_when_an_unrelated_earlier_line_changes() {
        // The anti-line-number property: changing line 1's content must NOT
        // change the anchor of a later, untouched line.
        let before = "AAA\nBBB\nCCC\n";
        let after = "ZZZ\nBBB\nCCC\n";
        assert_eq!(anchor_for(before, "CCC"), anchor_for(after, "CCC"));
        assert_eq!(anchor_for(before, "BBB"), anchor_for(after, "BBB"));
    }

    #[test]
    fn stale_anchor_errors_clearly() {
        let err = apply(
            SRC,
            &[Edit {
                op: Op::Replace,
                anchor: "dead".into(),
                text: Some("x".into()),
            }],
        )
        .unwrap_err();
        assert!(err.contains("stale or unknown anchor"));
    }

    #[test]
    fn conflicting_replace_and_delete_errors() {
        let a = anchor_for(SRC, "println");
        let err = apply(
            SRC,
            &[
                Edit {
                    op: Op::Replace,
                    anchor: a.clone(),
                    text: Some("x".into()),
                },
                Edit {
                    op: Op::Delete,
                    anchor: a,
                    text: None,
                },
            ],
        )
        .unwrap_err();
        assert!(err.contains("conflict"));
    }
}
