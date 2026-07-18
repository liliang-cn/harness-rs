//! [`SqlQueryTool`] — the agent-facing read-only SQL tool.

use crate::executor::{Row, SqlExecutor};
use crate::guard::{check_read_only, enforce_limit};
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use harness_redact::Redactor;
use serde_json::{Value, json};
use std::sync::Arc;

/// A read-only SQL query tool an agent can call to answer BI / production
/// questions against an ERP/MES backend. Enforces SELECT-only, appends a row
/// `LIMIT`, optionally redacts PII out of results, and returns rows as JSON.
/// Invalid or rejected SQL comes back as `ok: false` (not a hard error) so the
/// agent reads the reason and fixes its own query.
pub struct SqlQueryTool {
    executor: Arc<dyn SqlExecutor>,
    max_rows: u32,
    redactor: Option<Redactor>,
    schema: ToolSchema,
}

impl SqlQueryTool {
    /// Build the tool over an executor. The executor's `schema_doc` is folded
    /// into the tool description so the model knows the tables/columns.
    pub fn new(executor: Arc<dyn SqlExecutor>) -> Self {
        let schema = ToolSchema {
            name: "sql_query".into(),
            description: format!(
                "Run a SINGLE read-only SQL SELECT against the connected database and get \
                 rows back as JSON (row-capped). No writes, no DDL, no multiple statements. \
                 Prefer aggregates over fetching raw tables.\n\n{}",
                executor.schema_doc()
            ),
            input: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "A single SELECT statement. No semicolons, no writes."
                    }
                },
                "required": ["sql"]
            }),
        };
        Self {
            executor,
            max_rows: 200,
            redactor: None,
            schema,
        }
    }

    /// Cap on rows returned (also the value auto-appended as `LIMIT`). Default 200.
    pub fn with_max_rows(mut self, n: u32) -> Self {
        self.max_rows = n;
        self
    }

    /// Redact PII out of string values in the result rows before the agent (and
    /// any transcript/audit downstream) sees them.
    pub fn with_redactor(mut self, r: Redactor) -> Self {
        self.redactor = Some(r);
        self
    }

    fn redact(&self, rows: &mut [Row]) {
        let Some(r) = &self.redactor else { return };
        for row in rows.iter_mut() {
            for v in row.values_mut() {
                if let Value::String(s) = v {
                    *v = Value::String(r.scrub(s).text);
                }
            }
        }
    }
}

/// A refusal / failure surfaced to the model as a non-fatal result.
fn refuse(reason: String) -> ToolResult {
    ToolResult {
        ok: false,
        content: json!({ "error": reason }),
        trace: None,
    }
}

#[async_trait]
impl Tool for SqlQueryTool {
    fn name(&self) -> &str {
        "sql_query"
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }

    async fn invoke(&self, args: Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        let sql =
            args.get("sql")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs {
                    name: "sql_query".into(),
                    reason: "missing `sql` string".into(),
                })?;

        if let Err(e) = check_read_only(sql) {
            return Ok(refuse(e.to_string()));
        }
        let bounded = enforce_limit(sql, self.max_rows);

        match self.executor.query(&bounded).await {
            Ok(mut rows) => {
                let total = rows.len();
                let truncated = total > self.max_rows as usize;
                rows.truncate(self.max_rows as usize);
                self.redact(&mut rows);
                Ok(ToolResult {
                    ok: true,
                    content: json!({
                        "rows": rows,
                        "row_count": rows.len(),
                        "truncated": truncated,
                        "sql": bounded,
                    }),
                    trace: Some(format!("sql_query: {} rows", rows.len())),
                })
            }
            // A backend/SQL error is returned as ok:false so the agent can self-correct.
            Err(e) => Ok(refuse(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{Row, SqlError};
    use harness_context::default_world;

    /// A canned executor: records the SQL it was asked to run and returns fixed rows.
    struct MockExec {
        rows: Vec<Row>,
        last_sql: std::sync::Mutex<String>,
    }
    #[async_trait]
    impl SqlExecutor for MockExec {
        async fn query(&self, sql: &str) -> Result<Vec<Row>, SqlError> {
            *self.last_sql.lock().unwrap() = sql.to_string();
            Ok(self.rows.clone())
        }
        fn schema_doc(&self) -> &str {
            "Tables: work_orders(id, part_no, qty, defect_rate)"
        }
    }

    fn row(pairs: &[(&str, Value)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[tokio::test]
    async fn write_attempt_is_refused_not_executed() {
        let exec = Arc::new(MockExec {
            rows: vec![],
            last_sql: Default::default(),
        });
        let tool = SqlQueryTool::new(exec.clone());
        let mut w = default_world(std::env::temp_dir().join("sql-tool-test"));

        let res = tool
            .invoke(json!({ "sql": "DELETE FROM work_orders" }), &mut w)
            .await
            .unwrap();
        assert!(!res.ok, "write must be refused");
        assert!(
            exec.last_sql.lock().unwrap().is_empty(),
            "refused query must never reach the executor"
        );
    }

    #[tokio::test]
    async fn select_runs_with_limit_appended_and_redaction() {
        let exec = Arc::new(MockExec {
            rows: vec![row(&[
                ("part_no", json!("BRK-2049")),
                ("owner_email", json!("zhang@supplier.com")),
            ])],
            last_sql: Default::default(),
        });
        let tool = SqlQueryTool::new(exec.clone()).with_redactor(Redactor::new());
        let mut w = default_world(std::env::temp_dir().join("sql-tool-test2"));

        let res = tool
            .invoke(
                json!({ "sql": "SELECT part_no, owner_email FROM work_orders" }),
                &mut w,
            )
            .await
            .unwrap();
        assert!(res.ok);
        // LIMIT was appended before hitting the executor.
        assert!(
            exec.last_sql.lock().unwrap().contains("LIMIT 200"),
            "auto-LIMIT missing: {}",
            exec.last_sql.lock().unwrap()
        );
        // The email in the result was redacted.
        let email = res.content["rows"][0]["owner_email"].as_str().unwrap();
        assert!(
            !email.contains("zhang@supplier.com"),
            "email leaked: {email}"
        );
    }
}
