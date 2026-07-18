//! A [`SqlExecutor`] backed by SQLite via `sqlx` (feature `sqlite`).
//!
//! SQLite is the one backend we can exercise in CI without a server, so it's the
//! reference implementation. MySQL / PostgreSQL are the same shape (swap the
//! `sqlx` pool type); SQL Server needs a separate driver (`tiberius`) since
//! `sqlx` dropped MSSQL — implement [`SqlExecutor`] over it the same way.

use crate::executor::{Row, SqlError, SqlExecutor};
use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::sqlite::SqliteRow;
use sqlx::{Column, Row as _, SqlitePool, ValueRef};

/// Runs read-only queries against a SQLite database.
pub struct SqliteExecutor {
    pool: SqlitePool,
    schema_doc: String,
}

impl SqliteExecutor {
    /// Connect to `url` (e.g. `sqlite::memory:` or `sqlite:///data/mes.db`).
    /// For a real ERP mirror, point this at a read-only replica / user.
    pub async fn connect(url: &str, schema_doc: impl Into<String>) -> Result<Self, SqlError> {
        let pool = SqlitePool::connect(url)
            .await
            .map_err(|e| SqlError::Backend(e.to_string()))?;
        Ok(Self {
            pool,
            schema_doc: schema_doc.into(),
        })
    }

    /// Wrap an existing pool (e.g. one you seeded in a test).
    pub fn from_pool(pool: SqlitePool, schema_doc: impl Into<String>) -> Self {
        Self {
            pool,
            schema_doc: schema_doc.into(),
        }
    }
}

#[async_trait]
impl SqlExecutor for SqliteExecutor {
    async fn query(&self, sql: &str) -> Result<Vec<Row>, SqlError> {
        let rows = sqlx::query(sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| SqlError::Query(e.to_string()))?;
        rows.iter().map(row_to_json).collect()
    }

    fn schema_doc(&self) -> &str {
        &self.schema_doc
    }
}

fn row_to_json(row: &SqliteRow) -> Result<Row, SqlError> {
    let mut map = serde_json::Map::new();
    for col in row.columns() {
        let i = col.ordinal();
        let raw = row
            .try_get_raw(i)
            .map_err(|e| SqlError::Query(e.to_string()))?;
        // SQLite is dynamically typed and reports no usable type for computed
        // columns (e.g. `SUM(x) AS total`), so try storage classes in order
        // rather than trusting the declared type: INTEGER, then REAL, then TEXT.
        let value = if raw.is_null() {
            Value::Null
        } else if let Ok(n) = row.try_get::<i64, _>(i) {
            json!(n)
        } else if let Ok(f) = row.try_get::<f64, _>(i) {
            json!(f)
        } else if let Ok(s) = row.try_get::<String, _>(i) {
            json!(s)
        } else {
            // BLOB or anything else we can't render as JSON.
            Value::Null
        };
        map.insert(col.name().to_string(), value);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqlQueryTool;
    use harness_context::default_world;
    use harness_core::Tool;
    use serde_json::json;
    use std::sync::Arc;

    async fn seeded_mes() -> SqliteExecutor {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE work_orders (id INTEGER, part_no TEXT, qty INTEGER, defect_rate REAL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO work_orders VALUES (1, 'BRK-2049', 500, 0.012), (2, 'BRK-2049', 300, 0.031)")
            .execute(&pool)
            .await
            .unwrap();
        SqliteExecutor::from_pool(
            pool,
            "Tables: work_orders(id INT, part_no TEXT, qty INT, defect_rate REAL)",
        )
    }

    #[tokio::test]
    async fn queries_real_sqlite_through_the_tool() {
        let exec = Arc::new(seeded_mes().await);
        let tool = SqlQueryTool::new(exec);
        let mut w = default_world(std::env::temp_dir().join("sql-sqlite-test"));

        let res = tool
            .invoke(
                json!({ "sql": "SELECT part_no, SUM(qty) AS total FROM work_orders GROUP BY part_no" }),
                &mut w,
            )
            .await
            .unwrap();

        assert!(res.ok, "content: {}", res.content);
        assert_eq!(res.content["row_count"], 1);
        assert_eq!(res.content["rows"][0]["part_no"], "BRK-2049");
        assert_eq!(res.content["rows"][0]["total"], 800);
    }

    #[tokio::test]
    async fn write_never_reaches_sqlite() {
        let exec = Arc::new(seeded_mes().await);
        let tool = SqlQueryTool::new(exec);
        let mut w = default_world(std::env::temp_dir().join("sql-sqlite-test2"));

        let res = tool
            .invoke(json!({ "sql": "DROP TABLE work_orders" }), &mut w)
            .await
            .unwrap();
        assert!(!res.ok, "DROP must be refused");
    }
}
