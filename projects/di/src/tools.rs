//! 只读数据库工具:让 Agent 直接看当前所选库的表结构、并写只读 SQL 查真实数字。
//! 三重安全:① 只允许 SELECT/WITH,② 每次查询在 `READ ONLY` 事务里跑,③ 500 行上限。
use crate::db::DbHub;
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 一条真实执行过的查询。溯源必须由服务端记录:模型能编数字,就能编"证据"。
#[derive(Clone, serde::Serialize)]
pub struct Exec {
    pub query: String,
    pub source: String,
    pub rows: u64,
    pub ms: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip)]
    pub ts_ms: u128,
}

/// 进程内的查询日志(保留最近若干条),供 `GET /evidence?since=` 取用。
#[derive(Default)]
pub struct ExecLog(Mutex<Vec<Exec>>);

impl ExecLog {
    pub fn push(&self, e: Exec) {
        let mut v = self.0.lock().unwrap();
        v.push(e);
        let len = v.len();
        if len > 500 {
            v.drain(0..len - 500);
        }
    }
    pub fn since(&self, ts_ms: u128) -> Vec<Exec> {
        self.0.lock().unwrap().iter().filter(|e| e.ts_ms >= ts_ms).cloned().collect()
    }
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn ok(content: Value) -> ToolResult {
    ToolResult { ok: true, content, trace: None }
}
fn bad(content: Value) -> ToolResult {
    ToolResult { ok: false, content, trace: None }
}
fn exec(e: impl std::fmt::Display) -> ToolError {
    ToolError::Exec(e.to_string())
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
    hub: Arc<DbHub>,
    gov: Arc<crate::governed::Governed>,
    schema: ToolSchema,
}
impl ListTables {
    pub fn new(hub: Arc<DbHub>, gov: Arc<crate::governed::Governed>) -> Self {
        Self {
            hub,
            gov,
            schema: ToolSchema {
                name: "list_tables".into(),
                description: "列出当前数据库(public schema)里所有表及其列数。做任何分析前先用它了解有哪些数据。".into(),
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
        // 治理库:表结构不是可用口径,给了只会让模型在两套工具间摇摆
        let cur = self.hub.current().await;
        if self.gov.is_governed(&cur) {
            return Ok(bad(json!({"error": format!(
                "库 {cur} 走治理模式:请用 list_metrics 看指标、get_dimensions 看维度、query_metric 取数,不要看表结构")})));
        }
        let pool = self.hub.pool().await.map_err(exec)?;
        let tables = pool.list_tables().await.map_err(exec)?;
        Ok(ok(json!({ "database": self.hub.current().await, "tables": tables })))
    }
}

// ── describe_table ────────────────────────────────────────────────────────
pub struct DescribeTable {
    hub: Arc<DbHub>,
    gov: Arc<crate::governed::Governed>,
    schema: ToolSchema,
}
impl DescribeTable {
    pub fn new(hub: Arc<DbHub>, gov: Arc<crate::governed::Governed>) -> Self {
        Self {
            hub,
            gov,
            schema: ToolSchema {
                name: "describe_table".into(),
                description: "看某张表的列(名+类型)、总行数、和 3 行样例数据。写 SQL 前先了解列名和口径。".into(),
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
        let table = a.get("table").and_then(|v| v.as_str()).ok_or(ToolError::InvalidArgs {
            name: "describe_table".into(),
            reason: "缺少 table 参数".into(),
        })?;
        // 治理库:表结构不是可用口径,给了只会让模型在两套工具间摇摆
        let cur = self.hub.current().await;
        if self.gov.is_governed(&cur) {
            return Ok(bad(json!({"error": format!(
                "库 {cur} 走治理模式:请用 list_metrics 看指标、get_dimensions 看维度、query_metric 取数,不要看表结构")})));
        }
        let pool = self.hub.pool().await.map_err(exec)?;
        let columns = pool.columns_of(table).await.map_err(exec)?;
        if columns.is_empty() {
            return Ok(bad(json!({"error": format!("表 {table} 不存在")})));
        }
        let q = pool.kind().quote(table);
        let count = pool
            .scalar_f64(&format!("SELECT count(*) AS n FROM {q}"))
            .await
            .unwrap_or(0.0) as i64;
        let sample_rows = pool
            .query_json(&format!("SELECT * FROM {q} LIMIT 3"))
            .await
            .unwrap_or_default();
        Ok(ok(json!({"table": table, "row_count": count, "columns": columns, "sample_rows": sample_rows})))
    }
}

// ── run_sql ───────────────────────────────────────────────────────────────
pub struct RunSql {
    hub: Arc<DbHub>,
    log: Arc<ExecLog>,
    gov: Arc<crate::governed::Governed>,
    schema: ToolSchema,
}
impl RunSql {
    pub fn new(hub: Arc<DbHub>, log: Arc<ExecLog>, gov: Arc<crate::governed::Governed>) -> Self {
        Self {
            hub,
            log,
            gov,
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
        let sql = a.get("sql").and_then(|v| v.as_str()).ok_or(ToolError::InvalidArgs {
            name: "run_sql".into(),
            reason: "缺少 sql 参数".into(),
        })?;
        // 已建模的库走治理口径:模型不能自己写 SQL(这是结构性保证,不靠提示词)
        let cur = self.hub.current().await;
        if self.gov.is_governed(&cur) {
            return Ok(bad(json!({"error": format!(
                "库 {cur} 已配置语义模型,请用 query_metric 按已定义的指标查数(口径固定、跨 grain 正确、含权限与脱敏)")})));
        }
        if !is_read_only(sql) {
            return Ok(bad(json!({"error": "只允许只读查询(以 SELECT 或 WITH 开头、单条语句、无写操作)"})));
        }
        let pool = self.hub.pool().await.map_err(exec)?;
        let db = self.hub.current().await;
        let started = Instant::now();
        let wrapped = format!("SELECT * FROM ({}) _q LIMIT 500", sql.trim().trim_end_matches(';'));
        let res = pool.query_json_readonly(&wrapped).await;
        let ms = started.elapsed().as_millis() as u64;
        // 无论成败都记进查询日志——这是回答的可核查依据
        let mut entry = Exec {
            query: sql.trim().to_string(),
            source: db,
            rows: 0,
            ms,
            ok: res.is_ok(),
            error: None,
            ts_ms: now_ms(),
        };
        match res {
            Ok(out) => {
                entry.rows = out.len() as u64;
                self.log.push(entry);
                Ok(ok(json!({"row_count": out.len(), "rows": out})))
            }
            // 把 SQL 错误交回模型,让它自己改写重试
            Err(e) => {
                entry.error = Some(e.to_string());
                self.log.push(entry);
                Ok(bad(json!({"error": format!("SQL 执行失败: {e}"), "sql": sql})))
            }
        }
    }
}

// ── branch_diff ───────────────────────────────────────────────────────────
/// 只读工具:把「入库分支 vs 生产」的差异交给模型判断。模型只能读差异,
/// 建分支/合并/丢弃都是人工 HTTP 操作——模型永远写不到生产。
pub struct BranchDiff {
    hub: Arc<DbHub>,
    schema: ToolSchema,
}
impl BranchDiff {
    pub fn new(hub: Arc<DbHub>) -> Self {
        Self {
            hub,
            schema: ToolSchema {
                name: "branch_diff".into(),
                description: "对比某个入库分支与生产数据的差异(逐表行数、各数值列合计、可疑信号)。用于判断这批新数据能否入库。".into(),
                input: json!({"type":"object","properties":{"name":{"type":"string","description":"分支名"}},"required":["name"],"additionalProperties":false}),
            },
        }
    }
}
#[async_trait]
impl Tool for BranchDiff {
    fn name(&self) -> &str { &self.schema.name }
    fn schema(&self) -> &ToolSchema { &self.schema }
    fn risk(&self) -> ToolRisk { ToolRisk::ReadOnly }
    async fn invoke(&self, a: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        match crate::branch::diff(&self.hub, name).await {
            Ok(v) => Ok(ok(v)),
            Err(e) => Ok(bad(json!({"error": e.to_string()}))),
        }
    }
}
