//! The two read-only tools the quality agent uses. Both run against the
//! in-memory [`knowledge`](crate::knowledge) stand-ins; a real deployment swaps
//! the bodies for CortexDB calls without changing the agent.

use crate::knowledge;
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
use std::sync::OnceLock;

/// Searches controlled quality documents and returns **only current controlled
/// revisions** as answers, listing any matched obsolete revisions separately so
/// the agent never cites a superseded standard.
pub struct QualityDocSearch;

fn doc_schema() -> &'static ToolSchema {
    static S: OnceLock<ToolSchema> = OnceLock::new();
    S.get_or_init(|| ToolSchema {
        name: "quality_doc_search".into(),
        description: "Search controlled quality documents (SOP, inspection standards, 8D). \
             Returns only CURRENT controlled revisions as citable results; obsolete \
             revisions are reported separately and must not be cited. Filter by part_no \
             when known."
            .into(),
        input: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Keywords, e.g. 摩擦系数 检验标准."},
                "part_no": {"type": "string", "description": "Optional part number, e.g. BRK-2049."}
            },
            "required": ["query"]
        }),
    })
}

#[async_trait]
impl Tool for QualityDocSearch {
    fn name(&self) -> &str {
        "quality_doc_search"
    }
    fn schema(&self) -> &ToolSchema {
        doc_schema()
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }

    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let part_no = args.get("part_no").and_then(Value::as_str);
        let (current, obsolete) = knowledge::search_controlled(query, part_no);

        let citations: Vec<Value> = current
            .iter()
            .map(|h| {
                json!({
                    "doc_id": h.doc_id,
                    "title": h.title,
                    "revision": h.revision,
                    "controlled": true,
                    "excerpt": h.body,
                })
            })
            .collect();
        let excluded: Vec<Value> = obsolete
            .iter()
            .map(|h| json!({ "doc_id": h.doc_id, "revision": h.revision, "reason": "obsolete" }))
            .collect();

        Ok(ToolResult {
            ok: true,
            content: json!({
                "citations": citations,
                "excluded_obsolete": excluded,
                "note": "只可引用 citations 中的现行受控版本;excluded_obsolete 为过期版,禁止引用。"
            }),
            trace: None,
        })
    }
}

/// Traverses the manufacturing graph from a node (part / defect / process),
/// returning relation chains — the "which process/defect/8D relates to this
/// part" query that plain RAG can't answer.
pub struct QualityGraph;

fn graph_schema() -> &'static ToolSchema {
    static S: OnceLock<ToolSchema> = OnceLock::new();
    S.get_or_init(|| ToolSchema {
        name: "quality_graph".into(),
        description: "Traverse the manufacturing knowledge graph from a node (part number, \
             process, or defect) and return related chains: process → possible defect → \
             historical 8D / equipment. Use for change-impact and defect-history questions."
            .into(),
        input: json!({
            "type": "object",
            "properties": {
                "node": {"type": "string", "description": "Start node, e.g. BRK-2049 or 摩擦系数偏低."}
            },
            "required": ["node"]
        }),
    })
}

#[async_trait]
impl Tool for QualityGraph {
    fn name(&self) -> &str {
        "quality_graph"
    }
    fn schema(&self) -> &ToolSchema {
        graph_schema()
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }

    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        let node = args.get("node").and_then(Value::as_str).unwrap_or("");
        let chains = knowledge::related_chains(node);
        Ok(ToolResult {
            ok: true,
            content: json!({ "node": node, "chains": chains }),
            trace: None,
        })
    }
}
