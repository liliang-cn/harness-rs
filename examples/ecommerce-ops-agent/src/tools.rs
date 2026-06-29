//! A deliberately flaky external "market signal" tool. The first call fails
//! with a simulated upstream 503; retries succeed. It returns a synthetic
//! demand index for a SKU so the reorder deep-dive has an external signal to
//! reason about — and shows an agent coping with an unreliable dependency.

use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Default)]
pub struct MarketSignalTool {
    calls: AtomicU32,
}

impl MarketSignalTool {
    pub fn new() -> Self {
        Self::default()
    }
}

fn schema() -> &'static ToolSchema {
    static S: OnceLock<ToolSchema> = OnceLock::new();
    S.get_or_init(|| ToolSchema {
        name: "market_signal".into(),
        description: "Fetch an external market demand index (0-100) and a 30-day trend for a \
                      product SKU. This is a third-party API and is occasionally unavailable \
                      (HTTP 503) — if it fails, simply retry."
            .into(),
        input: json!({
            "type": "object",
            "properties": { "sku": {"type": "string"} },
            "required": ["sku"]
        }),
    })
}

#[async_trait]
impl Tool for MarketSignalTool {
    fn name(&self) -> &str {
        "market_signal"
    }
    fn schema(&self) -> &ToolSchema {
        schema()
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Network
    }

    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        let sku = args.get("sku").and_then(|v| v.as_str()).unwrap_or("");
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        // Fail the very first call to force a retry, then serve.
        if n == 0 {
            return Ok(ToolResult {
                ok: false,
                content: json!({"error": "503 Service Unavailable — market data upstream is busy, retry"}),
                trace: Some("market_signal 503".into()),
            });
        }
        // Deterministic pseudo-signal from the SKU bytes.
        let seed: u32 = sku.bytes().map(|b| b as u32).sum();
        let demand = 30 + (seed % 70); // 30..99
        let trend = match seed % 3 {
            0 => "rising",
            1 => "flat",
            _ => "declining",
        };
        Ok(ToolResult {
            ok: true,
            content: json!({"sku": sku, "demand_index": demand, "trend_30d": trend}),
            trace: Some(format!("market_signal {sku} -> {demand}")),
        })
    }
}
