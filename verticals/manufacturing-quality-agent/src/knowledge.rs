//! In-memory stand-ins for what a real deployment keeps in CortexDB: a set of
//! **controlled** quality documents (IATF 16949 style) and a small manufacturing
//! **graph** (part ↔ process ↔ defect ↔ 8D). Kept in-process so this runs with
//! no server, no network, no model — swap the bodies for CortexDB in production.

/// One controlled document. In IATF-land, an answer must cite the *current*
/// controlled revision; obsolete revisions must not be handed out as if valid.
pub struct ControlledDoc {
    pub doc_id: &'static str,
    pub title: &'static str,
    pub part_no: &'static str,
    pub revision: &'static str,
    /// `true` = current controlled version; `false` = superseded / obsolete.
    pub controlled: bool,
    pub body: &'static str,
}

/// The document register. Note two revisions of the same standard: Rev.B is
/// obsolete, Rev.C is the current controlled one — the search must never present
/// Rev.B as authoritative.
pub fn documents() -> Vec<ControlledDoc> {
    vec![
        ControlledDoc {
            doc_id: "QIS-BRK-2049",
            title: "前刹车片摩擦系数检验标准",
            part_no: "BRK-2049",
            revision: "B",
            controlled: false, // superseded by Rev.C
            body: "摩擦系数 0.35–0.45;试验温度 常温。",
        },
        ControlledDoc {
            doc_id: "QIS-BRK-2049",
            title: "前刹车片摩擦系数检验标准",
            part_no: "BRK-2049",
            revision: "C",
            controlled: true,
            body: "摩擦系数 0.38–0.42;试验温度 100±5℃;每批抽检 5 片并做 SPC。",
        },
        ControlledDoc {
            doc_id: "WI-SINTER-07",
            title: "烧结作业指导书",
            part_no: "BRK-2049",
            revision: "A",
            controlled: true,
            body: "烧结温度 1080±20℃;保温 45min;炉温异常需停线并通知工艺。",
        },
        ControlledDoc {
            doc_id: "8D-2024-017",
            title: "8D:前刹车片摩擦系数偏低",
            part_no: "BRK-2049",
            revision: "1",
            controlled: true,
            body: "根因:烧结炉温偏低导致摩擦系数偏低;纠正:上调炉温设定并增加 SPC 监控。",
        },
    ]
}

/// A hit from [`search_controlled`].
pub struct DocHit {
    pub doc_id: &'static str,
    pub title: &'static str,
    pub revision: &'static str,
    pub body: &'static str,
}

/// Search controlled documents. Returns `(current_hits, excluded_obsolete)` —
/// obsolete revisions that matched are reported separately, never as answers.
/// Matching is a simple substring check (a real deployment uses CortexDB hybrid
/// lexical+vector retrieval; part numbers need exact lexical match).
pub fn search_controlled(query: &str, part_no: Option<&str>) -> (Vec<DocHit>, Vec<DocHit>) {
    let mut current = Vec::new();
    let mut obsolete = Vec::new();
    for d in documents() {
        if let Some(p) = part_no
            && d.part_no != p
        {
            continue;
        }
        let matches = query.is_empty()
            || d.title.contains(query)
            || d.body.contains(query)
            || query
                .split_whitespace()
                .any(|t| d.body.contains(t) || d.title.contains(t));
        if !matches {
            continue;
        }
        let hit = DocHit {
            doc_id: d.doc_id,
            title: d.title,
            revision: d.revision,
            body: d.body,
        };
        if d.controlled {
            current.push(hit);
        } else {
            obsolete.push(hit);
        }
    }
    (current, obsolete)
}

/// Manufacturing knowledge graph as `(subject, relation, object)` triples. This
/// is the differentiator over plain RAG: relationship queries like "which defect
/// relates to which process / which historical 8D".
fn triples() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("BRK-2049", "工序", "烧结"),
        ("BRK-2049", "用于车型", "汉EV"),
        ("BRK-2049", "供给客户", "某主机厂"),
        ("烧结", "可能不良", "摩擦系数偏低"),
        ("摩擦系数偏低", "相关8D", "8D-2024-017"),
        ("摩擦系数偏低", "涉及设备", "烧结炉#3"),
    ]
}

/// All root-to-leaf relation chains reachable from `start` (depth-first). Renders
/// human-readable paths like `BRK-2049 —工序→ 烧结 —可能不良→ 摩擦系数偏低 …`.
pub fn related_chains(start: &str) -> Vec<String> {
    let edges = triples();
    let mut out = Vec::new();
    dfs(start, &edges, start, &mut out);
    // Keep only multi-hop chains (a lone node isn't interesting).
    out.into_iter().filter(|c| c.contains('→')).collect()
}

fn dfs(
    node: &str,
    edges: &[(&'static str, &'static str, &'static str)],
    path: &str,
    out: &mut Vec<String>,
) {
    let children: Vec<_> = edges.iter().filter(|(s, _, _)| *s == node).collect();
    if children.is_empty() {
        out.push(path.to_string());
        return;
    }
    for (_, rel, obj) in children {
        dfs(obj, edges, &format!("{path} —{rel}→ {obj}"), out);
    }
}
