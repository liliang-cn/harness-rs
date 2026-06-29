//! A read-only SQL tool the analyst agents use to query the live database.
//!
//! The model writes a `SELECT`, the tool runs it against Postgres and returns
//! the rows as JSON. SQL errors come back as `ok: false` (not a hard tool
//! error) so the agent can read the message and fix its query itself.

use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::sync::OnceLock;

/// Schema text injected into every analyst prompt so the model writes valid
/// SQL on the first try.
pub const SCHEMA_DOC: &str = "\
Tables (PostgreSQL):
  products(id, sku, name, category, brand, unit_price_cents, unit_cost_cents, stock_qty, reorder_level, created_at)
  customers(id, name, email, city, country, segment[new|returning|vip], created_at)
  orders(id, customer_id, status[completed|shipped|refunded|cancelled], channel[web|mobile|marketplace], ordered_at, total_cents)
  order_items(id, order_id, product_id, qty, unit_price_cents)
  reviews(id, product_id, customer_id, rating[1-5], title, body, created_at)
Money is in integer cents. Revenue = SUM(order_items.qty * order_items.unit_price_cents) for orders with status='completed'.
Margin uses products.unit_cost_cents. 'Now' is the max(ordered_at) in the data.";

pub struct SqlQueryTool {
    pool: PgPool,
}

impl SqlQueryTool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn schema() -> &'static ToolSchema {
    static S: OnceLock<ToolSchema> = OnceLock::new();
    S.get_or_init(|| ToolSchema {
        name: "sql_query".into(),
        description: format!(
            "Run a single read-only SQL SELECT against the shop's PostgreSQL database and \
             get the rows back as JSON (capped at 500 rows). Use aggregates; do not fetch \
             raw tables. {SCHEMA_DOC}"
        ),
        input: json!({
            "type": "object",
            "properties": {
                "sql": {"type": "string", "description": "A single SELECT statement (no semicolons, no writes)."}
            },
            "required": ["sql"]
        }),
    })
}

#[async_trait]
impl Tool for SqlQueryTool {
    fn name(&self) -> &str {
        "sql_query"
    }
    fn schema(&self) -> &ToolSchema {
        schema()
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }

    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        let sql = args
            .get("sql")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "sql_query".into(),
                reason: "missing string field `sql`".into(),
            })?
            .trim()
            .trim_end_matches(';')
            .to_string();

        // Read-only guard: must be a single SELECT/WITH, no statement breaks.
        let lower = sql.to_lowercase();
        let starts_ok = lower.starts_with("select") || lower.starts_with("with");
        if !starts_ok || sql.contains(';') {
            return Ok(ToolResult {
                ok: false,
                content: json!({"error": "only a single read-only SELECT (or WITH) is allowed"}),
                trace: None,
            });
        }

        // Delegate all type conversion to Postgres: wrap as json_agg::text.
        let wrapped = format!(
            "SELECT COALESCE(json_agg(row_to_json(t)), '[]')::text AS data \
             FROM (SELECT * FROM ({sql}) sub LIMIT 500) t"
        );

        match sqlx::query(&wrapped).fetch_one(&self.pool).await {
            Ok(row) => {
                let data: String = row.try_get("data").unwrap_or_else(|_| "[]".into());
                let rows: Value = serde_json::from_str(&data).unwrap_or_else(|_| json!([]));
                let n = rows.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult {
                    ok: true,
                    content: json!({"row_count": n, "rows": rows}),
                    trace: Some(format!("sql_query -> {n} rows")),
                })
            }
            Err(e) => Ok(ToolResult {
                ok: false,
                content: json!({"error": e.to_string()}),
                trace: Some("sql_query error".into()),
            }),
        }
    }
}
