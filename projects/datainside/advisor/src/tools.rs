//! 只读数据库工具:让 Agent 直接看客户库的表结构、并写只读 SQL 查真实数字。
//! 三重安全:① 只允许 SELECT/WITH,② 每次查询在 `READ ONLY` 事务里跑,③ 500 行上限。
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::sync::Arc;

fn ok(content: Value) -> ToolResult {
    ToolResult {
        ok: true,
        content,
        trace: None,
    }
}
fn bad(content: Value) -> ToolResult {
    ToolResult {
        ok: false,
        content,
        trace: None,
    }
}
fn exec(e: impl std::fmt::Display) -> ToolError {
    ToolError::Exec(e.to_string())
}
fn qident(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

/// 只允许只读查询:必须以 SELECT/WITH 开头,不含分号(挡叠加语句)与写/DDL 关键字。
/// (真正的保险是 READ ONLY 事务;这里是纵深防御。)
fn is_read_only(sql: &str) -> bool {
    let s = sql.trim().trim_end_matches(';').trim().to_lowercase();
    if !(s.starts_with("select") || s.starts_with("with")) {
        return false;
    }
    if s.contains(';') {
        return false;
    }
    const BANNED: &[&str] = &[
        "insert ", "update ", "delete ", "drop ", "alter ", "create ", "truncate", "grant ",
        "revoke ", " into ", "copy ", "merge ",
    ];
    !BANNED.iter().any(|b| s.contains(b))
}

// ── list_tables ───────────────────────────────────────────────────────────
pub struct ListTables {
    pool: Arc<PgPool>,
    schema: ToolSchema,
}
impl ListTables {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self {
            pool,
            schema: ToolSchema {
                name: "list_tables".into(),
                description:
                    "列出数据库(public schema)里所有表及其列数。做任何分析前先用它了解有哪些数据。"
                        .into(),
                input: json!({"type":"object","properties":{},"additionalProperties":false}),
            },
        }
    }
}
#[async_trait]
impl Tool for ListTables {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, _a: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let rows = sqlx::query(
            "SELECT t.table_name,
               (SELECT count(*) FROM information_schema.columns c
                WHERE c.table_name=t.table_name AND c.table_schema='public') AS cols
             FROM information_schema.tables t
             WHERE t.table_schema='public' AND t.table_type='BASE TABLE'
             ORDER BY t.table_name",
        )
        .fetch_all(&*self.pool)
        .await
        .map_err(exec)?;
        let tables: Vec<Value> = rows
            .iter()
            .map(|r| json!({"table": r.get::<String,_>("table_name"), "columns": r.get::<i64,_>("cols")}))
            .collect();
        Ok(ok(json!({ "tables": tables })))
    }
}

// ── describe_table ────────────────────────────────────────────────────────
pub struct DescribeTable {
    pool: Arc<PgPool>,
    schema: ToolSchema,
}
impl DescribeTable {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self {
            pool,
            schema: ToolSchema {
                name: "describe_table".into(),
                description:
                    "看某张表的列(名+类型)、总行数、和 3 行样例数据。写 SQL 前先了解列名和口径。"
                        .into(),
                input: json!({"type":"object","properties":{"table":{"type":"string","description":"表名"}},"required":["table"],"additionalProperties":false}),
            },
        }
    }
}
#[async_trait]
impl Tool for DescribeTable {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, a: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let table = a
            .get("table")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::InvalidArgs {
                name: "describe_table".into(),
                reason: "缺少 table 参数".into(),
            })?;
        let cols = sqlx::query(
            "SELECT column_name, data_type FROM information_schema.columns
             WHERE table_schema='public' AND table_name=$1 ORDER BY ordinal_position",
        )
        .bind(table)
        .fetch_all(&*self.pool)
        .await
        .map_err(exec)?;
        if cols.is_empty() {
            return Ok(bad(json!({"error": format!("表 {table} 不存在")})));
        }
        let columns: Vec<Value> = cols
            .iter()
            .map(|r| json!({"name": r.get::<String,_>("column_name"), "type": r.get::<String,_>("data_type")}))
            .collect();
        let count: i64 = sqlx::query(&format!("SELECT count(*) AS n FROM {}", qident(table)))
            .fetch_one(&*self.pool)
            .await
            .map_err(exec)?
            .get("n");
        let sample = sqlx::query(&format!(
            "SELECT to_jsonb(x) AS r FROM (SELECT * FROM {} LIMIT 3) x",
            qident(table)
        ))
        .fetch_all(&*self.pool)
        .await
        .map_err(exec)?;
        let sample_rows: Vec<Value> = sample.iter().map(|r| r.get::<Value, _>("r")).collect();
        Ok(ok(
            json!({"table": table, "row_count": count, "columns": columns, "sample_rows": sample_rows}),
        ))
    }
}

// ── run_sql ───────────────────────────────────────────────────────────────
pub struct RunSql {
    pool: Arc<PgPool>,
    schema: ToolSchema,
}
impl RunSql {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self {
            pool,
            schema: ToolSchema {
                name: "run_sql".into(),
                description: "执行一条只读 SQL(SELECT/WITH)查真实数字。可多次调用逐步深入。结果最多 500 行。用它取每个判断的数据支撑。".into(),
                input: json!({"type":"object","properties":{"sql":{"type":"string","description":"一条只读 SELECT 查询"}},"required":["sql"],"additionalProperties":false}),
            },
        }
    }
}
#[async_trait]
impl Tool for RunSql {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(&self, a: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let sql = a
            .get("sql")
            .and_then(|v| v.as_str())
            .ok_or(ToolError::InvalidArgs {
                name: "run_sql".into(),
                reason: "缺少 sql 参数".into(),
            })?;
        if !is_read_only(sql) {
            return Ok(bad(
                json!({"error": "只允许只读查询(以 SELECT 或 WITH 开头、单条语句、无写操作)"}),
            ));
        }
        let mut tx = self.pool.begin().await.map_err(exec)?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(exec)?;
        let wrapped = format!(
            "SELECT to_jsonb(_q) AS r FROM ({}) _q LIMIT 500",
            sql.trim().trim_end_matches(';')
        );
        let res = sqlx::query(&wrapped).fetch_all(&mut *tx).await;
        let _ = tx.rollback().await;
        match res {
            Ok(rows) => {
                let out: Vec<Value> = rows.iter().map(|r| r.get::<Value, _>("r")).collect();
                Ok(ok(json!({"row_count": out.len(), "rows": out})))
            }
            // 把 SQL 错误交回模型,让它自己改写重试(和治理层一个思路)
            Err(e) => Ok(bad(
                json!({"error": format!("SQL 执行失败: {e}"), "sql": sql}),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_read_only;

    #[test]
    fn accepts_single_read_only_queries() {
        assert!(is_read_only("SELECT count(*) FROM stores"));
        assert!(is_read_only(
            "WITH totals AS (SELECT 1 AS n) SELECT * FROM totals;"
        ));
    }

    #[test]
    fn rejects_writes_and_stacked_statements() {
        assert!(!is_read_only("DELETE FROM stores"));
        assert!(!is_read_only("SELECT 1; DROP TABLE stores"));
        assert!(!is_read_only(
            "WITH removed AS (DELETE FROM stores RETURNING *) SELECT * FROM removed"
        ));
        assert!(!is_read_only("SELECT * INTO backup FROM stores"));
    }
}
