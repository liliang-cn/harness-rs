//! The structured operational action the analysis proposes, plus parsing
//! (lenient JSON extraction from the synthesis agent's text) and the
//! blast-radius classification that drives governance.

use harness_loop_engine::LoopLevel;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpsAction {
    /// "reorder" | "markdown" | "pause_product"
    pub kind: String,
    pub sku: String,
    /// reorder quantity (for reorder)
    #[serde(default)]
    pub qty: Option<i32>,
    /// markdown percentage (for markdown)
    #[serde(default)]
    pub pct: Option<i32>,
    /// why — the data point justifying it
    #[serde(default)]
    pub reason: String,
}

impl OpsAction {
    /// Normalized gate key + blast-radius level.
    ///
    /// Low blast radius (reversible, bounded) auto-applies at L3; anything
    /// bigger escalates at L2.
    pub fn gate_kind_and_level(&self) -> (&'static str, LoopLevel) {
        match self.kind.as_str() {
            "reorder" => ("reorder", LoopLevel::L3Unattended),
            "markdown" if self.pct.unwrap_or(100) <= 20 => {
                ("markdown-small", LoopLevel::L3Unattended)
            }
            "markdown" => ("markdown-large", LoopLevel::L2Assisted),
            _ => ("pause_product", LoopLevel::L2Assisted),
        }
    }

    pub fn human(&self) -> String {
        match self.kind.as_str() {
            "reorder" => format!("reorder {} units of {}", self.qty.unwrap_or(0), self.sku),
            "markdown" => format!("mark {} down {}%", self.sku, self.pct.unwrap_or(0)),
            _ => format!("pause product {}", self.sku),
        }
    }
}

/// Coerce a JSON value to i32, accepting numbers or numeric strings like
/// `"120"` / `"30%"` (models are inconsistent about this).
fn as_i32(v: &serde_json::Value) -> Option<i32> {
    v.as_i64().map(|x| x as i32).or_else(|| {
        v.as_str()
            .and_then(|s| s.trim().trim_end_matches('%').trim().parse::<i32>().ok())
    })
}

/// Extract a JSON array of actions from free-form model text. Tolerant: finds
/// the first `[` … last `]`, parses that, and reads each field leniently so a
/// stringly-typed `qty`/`pct` still comes through.
pub fn parse_actions(text: &str) -> Vec<OpsAction> {
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else {
        return Vec::new();
    };
    if end <= start {
        return Vec::new();
    }
    let values: Vec<serde_json::Value> = match serde_json::from_str(&text[start..=end]) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    values
        .into_iter()
        .filter_map(|v| {
            let kind = v.get("kind").and_then(|k| k.as_str())?.trim().to_string();
            let sku = v.get("sku").and_then(|s| s.as_str())?.trim().to_string();
            if kind.is_empty() || sku.is_empty() {
                return None;
            }
            Some(OpsAction {
                kind,
                sku,
                qty: v.get("qty").and_then(as_i32),
                pct: v.get("pct").and_then(as_i32),
                reason: v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions_from_messy_text() {
        let text = r#"Here is my plan:
        [
          {"kind":"reorder","sku":"TOY-0046","qty":120,"reason":"4 days of cover"},
          {"kind":"markdown","sku":"HOM-0010","pct":30,"reason":"dead stock"},
          {"kind":"pause_product","sku":"SPT-0029","reason":"1.9 star rating"}
        ]
        Done."#;
        let acts = parse_actions(text);
        assert_eq!(acts.len(), 3);
        assert_eq!(acts[0].kind, "reorder");
        assert_eq!(acts[0].qty, Some(120));
    }

    #[test]
    fn classification_by_blast_radius() {
        let reorder = OpsAction {
            kind: "reorder".into(),
            sku: "X".into(),
            qty: Some(10),
            pct: None,
            reason: String::new(),
        };
        assert_eq!(reorder.gate_kind_and_level().1, LoopLevel::L3Unattended);

        let small = OpsAction {
            kind: "markdown".into(),
            sku: "X".into(),
            qty: None,
            pct: Some(15),
            reason: String::new(),
        };
        assert_eq!(
            small.gate_kind_and_level(),
            ("markdown-small", LoopLevel::L3Unattended)
        );

        let big = OpsAction {
            kind: "markdown".into(),
            sku: "X".into(),
            qty: None,
            pct: Some(40),
            reason: String::new(),
        };
        assert_eq!(
            big.gate_kind_and_level(),
            ("markdown-large", LoopLevel::L2Assisted)
        );

        let pause = OpsAction {
            kind: "pause_product".into(),
            sku: "X".into(),
            qty: None,
            pct: None,
            reason: String::new(),
        };
        assert_eq!(pause.gate_kind_and_level().1, LoopLevel::L2Assisted);
    }
}
