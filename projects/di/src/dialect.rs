//! 数据库方言:同一套上层逻辑跑 Postgres / MySQL / SQLite。
//!
//! 只支持 Postgres 会挡掉大半中小企业客户——他们的 ERP/POS 多是 MySQL,
//! 而单机工具和导出的数据常常就是一个 SQLite 文件。
//!
//! 三处必须按方言分开:
//! - **标识符引号**:Postgres/SQLite 用 `"x"`,MySQL 用 `` `x` ``。
//! - **元数据来源**:Postgres/MySQL 有 `information_schema`,SQLite 只有 `pragma` 和 `sqlite_master`。
//! - **行转 JSON**:Postgres 有 `to_jsonb(row)`,另两个没有。所以不靠数据库,
//!   在 Rust 里逐列取值转换——三边通用,也少一层对数据库函数的依赖。
use serde_json::{Map, Value, json};
use sqlx::{Column, Row, TypeInfo, ValueRef};

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Postgres,
    Mysql,
    Sqlite,
}

impl Kind {
    pub fn from_dsn(dsn: &str) -> Self {
        if dsn.starts_with("mysql://") || dsn.starts_with("mariadb://") {
            Kind::Mysql
        } else if dsn.starts_with("sqlite:") || dsn.ends_with(".db") || dsn.ends_with(".sqlite") {
            Kind::Sqlite
        } else {
            Kind::Postgres
        }
    }

    /// SQLite 是一个文件就是一个库——没有服务器,也没有「切库」。
    pub fn is_file_based(self) -> bool {
        self == Kind::Sqlite
    }
    pub fn label(self) -> &'static str {
        match self {
            Kind::Postgres => "PostgreSQL",
            Kind::Mysql => "MySQL",
            Kind::Sqlite => "SQLite",
        }
    }
    pub fn default_port(self) -> u16 {
        match self {
            Kind::Postgres => 5432,
            Kind::Mysql => 3306,
            Kind::Sqlite => 0,
        }
    }
    /// 标识符引号:两边不同,拼 SQL 时必须用对的那种。
    pub fn quote(self, id: &str) -> String {
        match self {
            // SQLite 也认双引号标识符
            Kind::Postgres | Kind::Sqlite => format!("\"{}\"", id.replace('"', "\"\"")),
            Kind::Mysql => format!("`{}`", id.replace('`', "``")),
        }
    }
}

/// 一个连接池,按方言分派。
#[derive(Clone)]
pub enum Pool {
    Pg(sqlx::PgPool),
    My(sqlx::MySqlPool),
    Sq(sqlx::SqlitePool),
}

impl Pool {
    pub async fn connect(dsn: &str, max: u32) -> anyhow::Result<Self> {
        Ok(match Kind::from_dsn(dsn) {
            Kind::Postgres => Pool::Pg(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(max)
                    .connect(dsn)
                    .await?,
            ),
            Kind::Mysql => Pool::My(
                sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(max)
                    .connect(dsn)
                    .await?,
            ),
            // 文件库:统一加 sqlite: 前缀,并以只读打开——本产品只查不写
            Kind::Sqlite => {
                let url = if dsn.starts_with("sqlite:") {
                    dsn.to_string()
                } else {
                    format!("sqlite://{dsn}")
                };
                let url = if url.contains("mode=") {
                    url
                } else if url.contains('?') {
                    format!("{url}&mode=ro")
                } else {
                    format!("{url}?mode=ro")
                };
                Pool::Sq(
                    sqlx::sqlite::SqlitePoolOptions::new()
                        .max_connections(max)
                        .connect(&url)
                        .await?,
                )
            }
        })
    }

    pub fn kind(&self) -> Kind {
        match self {
            Pool::Pg(_) => Kind::Postgres,
            Pool::My(_) => Kind::Mysql,
            Pool::Sq(_) => Kind::Sqlite,
        }
    }

    pub async fn close(&self) {
        match self {
            Pool::Pg(p) => p.close().await,
            Pool::My(p) => p.close().await,
            Pool::Sq(p) => p.close().await,
        }
    }

    /// 查询并把每行转成 JSON 对象。不依赖 `to_jsonb`,两种库通用。
    pub async fn query_json(&self, sql: &str) -> anyhow::Result<Vec<Value>> {
        Ok(match self {
            Pool::Pg(p) => sqlx::query(sql)
                .fetch_all(p)
                .await?
                .iter()
                .map(pg_row_to_json)
                .collect(),
            Pool::My(p) => sqlx::query(sql)
                .fetch_all(p)
                .await?
                .iter()
                .map(my_row_to_json)
                .collect(),
            Pool::Sq(p) => sqlx::query(sql)
                .fetch_all(p)
                .await?
                .iter()
                .map(sq_row_to_json)
                .collect(),
        })
    }

    /// 只读查询:在只读事务里跑,查完回滚——纵深防御,不指望上层的关键字过滤。
    pub async fn query_json_readonly(&self, sql: &str) -> anyhow::Result<Vec<Value>> {
        match self {
            Pool::Pg(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("SET TRANSACTION READ ONLY")
                    .execute(&mut *tx)
                    .await?;
                let res = sqlx::query(sql).fetch_all(&mut *tx).await;
                let _ = tx.rollback().await;
                Ok(res?.iter().map(pg_row_to_json).collect())
            }
            Pool::My(p) => {
                let mut tx = p.begin().await?;
                // MySQL 的只读事务:START TRANSACTION READ ONLY(5.6+)
                sqlx::query("SET TRANSACTION READ ONLY")
                    .execute(&mut *tx)
                    .await
                    .ok(); // 老版本不支持就退回普通事务 + 回滚
                let res = sqlx::query(sql).fetch_all(&mut *tx).await;
                let _ = tx.rollback().await;
                Ok(res?.iter().map(my_row_to_json).collect())
            }
            // 连接本身就是 mode=ro,再加 query_only 双保险
            Pool::Sq(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("PRAGMA query_only = ON")
                    .execute(&mut *tx)
                    .await
                    .ok();
                let res = sqlx::query(sql).fetch_all(&mut *tx).await;
                let _ = tx.rollback().await;
                Ok(res?.iter().map(sq_row_to_json).collect())
            }
        }
    }

    pub async fn scalar_f64(&self, sql: &str) -> Option<f64> {
        let rows = self.query_json(sql).await.ok()?;
        rows.first()?
            .as_object()?
            .values()
            .next()
            .and_then(json_as_f64)
    }

    pub async fn execute(&self, sql: &str) -> anyhow::Result<()> {
        match self {
            Pool::Pg(p) => {
                sqlx::query(sql).execute(p).await?;
            }
            Pool::My(p) => {
                sqlx::query(sql).execute(p).await?;
            }
            Pool::Sq(p) => {
                sqlx::query(sql).execute(p).await?;
            }
        }
        Ok(())
    }

    /// 实例上可选的业务库(排除系统库)。
    pub async fn list_databases(&self) -> anyhow::Result<Vec<Value>> {
        let sql = match self.kind() {
            Kind::Postgres =>
                "SELECT datname AS name, pg_size_pretty(pg_database_size(datname)) AS size
                 FROM pg_database WHERE datistemplate = false AND datname <> 'postgres'
                 ORDER BY datname",
            Kind::Mysql =>
                "SELECT s.schema_name AS name,
                        COALESCE(CONCAT(ROUND(SUM(t.data_length + t.index_length)/1024/1024, 1), ' MB'), '—') AS size
                 FROM information_schema.schemata s
                 LEFT JOIN information_schema.tables t ON t.table_schema = s.schema_name
                 WHERE s.schema_name NOT IN ('information_schema','mysql','performance_schema','sys')
                 GROUP BY s.schema_name ORDER BY s.schema_name",
            // 一个文件就是一个库:列出「自己」,大小取页数×页大小
            Kind::Sqlite => {
                let rows = self
                    .query_json(
                        "SELECT COALESCE(NULLIF(name,''),'main') AS name,
                                (SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()) AS bytes
                         FROM pragma_database_list() WHERE seq = 0",
                    )
                    .await?;
                return Ok(rows
                    .iter()
                    .map(|r| {
                        let b = r.get("bytes").and_then(json_as_f64).unwrap_or(0.0);
                        json!({
                            "name": r.get("name").and_then(|v| v.as_str()).unwrap_or("main"),
                            "size": format!("{:.1} MB", b / 1024.0 / 1024.0),
                        })
                    })
                    .collect());
            }
        };
        self.query_json(sql).await
    }

    /// 当前库里的表 + 列数。Postgres 看 public schema,MySQL 看当前 database。
    pub async fn list_tables(&self) -> anyhow::Result<Vec<Value>> {
        let sql = match self.kind() {
            Kind::Postgres =>
                "SELECT t.table_name AS \"table\",
                   (SELECT count(*) FROM information_schema.columns c
                    WHERE c.table_name = t.table_name AND c.table_schema = 'public') AS columns
                 FROM information_schema.tables t
                 WHERE t.table_schema='public' AND t.table_type='BASE TABLE'
                 ORDER BY t.table_name",
            Kind::Mysql =>
                "SELECT t.table_name AS `table`,
                   (SELECT count(*) FROM information_schema.columns c
                    WHERE c.table_name = t.table_name AND c.table_schema = DATABASE()) AS columns
                 FROM information_schema.tables t
                 WHERE t.table_schema = DATABASE() AND t.table_type='BASE TABLE'
                 ORDER BY t.table_name",
            // SQLite 没有 information_schema
            Kind::Sqlite =>
                "SELECT m.name AS \"table\",
                        (SELECT count(*) FROM pragma_table_info(m.name)) AS columns
                 FROM sqlite_master m
                 WHERE m.type='table' AND m.name NOT LIKE 'sqlite_%'
                 ORDER BY m.name",
        };
        self.query_json(sql).await
    }

    /// 某表的列名与类型。
    pub async fn columns_of(&self, table: &str) -> anyhow::Result<Vec<Value>> {
        let lit = sql_string(table);
        let sql = match self.kind() {
            Kind::Postgres => format!(
                "SELECT column_name AS name, data_type AS type FROM information_schema.columns
                 WHERE table_schema='public' AND table_name={lit} ORDER BY ordinal_position"
            ),
            Kind::Mysql => format!(
                "SELECT column_name AS name, data_type AS type FROM information_schema.columns
                 WHERE table_schema=DATABASE() AND table_name={lit} ORDER BY ordinal_position"
            ),
            Kind::Sqlite => format!(
                "SELECT name, type FROM pragma_table_info({lit}) ORDER BY cid"
            ),
        };
        self.query_json(&sql).await
    }

    /// 数值列(用于入库分支的合计对比)。
    pub async fn numeric_columns_of(&self, table: &str) -> anyhow::Result<Vec<String>> {
        let lit = sql_string(table);
        let sql = match self.kind() {
            Kind::Postgres => format!(
                "SELECT column_name AS name FROM information_schema.columns
                 WHERE table_schema='public' AND table_name={lit}
                   AND data_type IN ('integer','bigint','numeric','double precision','real','smallint')
                 ORDER BY ordinal_position"
            ),
            Kind::Mysql => format!(
                "SELECT column_name AS name FROM information_schema.columns
                 WHERE table_schema=DATABASE() AND table_name={lit}
                   AND data_type IN ('int','bigint','decimal','double','float','smallint','tinyint','mediumint')
                 ORDER BY ordinal_position"
            ),
            // SQLite 是动态类型,按声明的类型名匹配
            Kind::Sqlite => format!(
                "SELECT name FROM pragma_table_info({lit})
                 WHERE UPPER(type) LIKE '%INT%' OR UPPER(type) LIKE '%REAL%'
                    OR UPPER(type) LIKE '%NUMERIC%' OR UPPER(type) LIKE '%DECIMAL%'
                    OR UPPER(type) LIKE '%DOUBLE%' OR UPPER(type) LIKE '%FLOAT%'
                 ORDER BY cid"
            ),
        };
        Ok(self
            .query_json(&sql)
            .await?
            .iter()
            .filter_map(|v| v.get("name")?.as_str().map(str::to_string))
            .collect())
    }
}

/// 单引号字符串字面量(表名来自 information_schema,仍然转义以防万一)。
pub fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn json_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

// ── 行 → JSON ───────────────────────────────────────────────────────────────
// 逐列按类型解码。数据库类型千奇百怪,解不出来的一律退回字符串,
// 宁可给模型一个可读的值,也不要整行失败。

macro_rules! row_to_json {
    ($fn_name:ident, $row_ty:ty, $decimal:expr) => {
        fn $fn_name(row: &$row_ty) -> Value {
            let mut obj = Map::new();
            for (i, col) in row.columns().iter().enumerate() {
                let name = col.name().to_string();
                let is_null = row.try_get_raw(i).map(|r| r.is_null()).unwrap_or(true);
                if is_null {
                    obj.insert(name, Value::Null);
                    continue;
                }
                let ty = col.type_info().name().to_uppercase();
                let v = if ty.contains("BOOL") {
                    row.try_get::<bool, _>(i).map(Value::from).ok()
                } else if ty.contains("INT") || ty.contains("SERIAL") {
                    row.try_get::<i64, _>(i)
                        .map(Value::from)
                        .or_else(|_| row.try_get::<i32, _>(i).map(Value::from))
                        .ok()
                } else if ty.contains("FLOAT") || ty.contains("DOUBLE") || ty.contains("REAL") {
                    row.try_get::<f64, _>(i).map(Value::from).ok()
                } else if ty.contains("NUMERIC") || ty.contains("DECIMAL") {
                    // 金额常是 decimal:保精度地转出来(SQLite 无此类型,退回 f64)
                    ($decimal)(row, i)
                } else if ty.contains("JSON") {
                    row.try_get::<Value, _>(i).ok()
                } else if ty.contains("TIMESTAMP") || ty.contains("DATETIME") {
                    row.try_get::<chrono::NaiveDateTime, _>(i)
                        .map(|d| Value::from(d.to_string()))
                        .ok()
                } else if ty.contains("DATE") {
                    row.try_get::<chrono::NaiveDate, _>(i)
                        .map(|d| Value::from(d.to_string()))
                        .ok()
                } else {
                    None // 类型未知(如 SQLite 的聚合列),走下面的顺序尝试
                };
                // 兜底:按 整数 → 浮点 → 文本 → 字节 依次试。
                // SQLite 对 SUM()/COUNT() 这类计算列不报声明类型,只按字符串取会失败,
                // 结果整列变 null——模型会当成「数据缺失」,比报错更糟。
                obj.insert(
                    name,
                    v.or_else(|| row.try_get::<i64, _>(i).map(Value::from).ok())
                        .or_else(|| row.try_get::<f64, _>(i).map(Value::from).ok())
                        .or_else(|| row.try_get::<String, _>(i).map(Value::from).ok())
                        // MySQL 的 information_schema 返回 VARBINARY,取 String 会失败
                        .or_else(|| {
                            row.try_get::<Vec<u8>, _>(i)
                                .map(|b| Value::from(String::from_utf8_lossy(&b).to_string()))
                                .ok()
                        })
                        .unwrap_or(json!(format!("<{ty}>"))),
                );
            }
            Value::Object(obj)
        }
    };
}

/// Postgres/MySQL 有真正的 decimal:转字符串保精度,别让 f64 吃掉小数。
macro_rules! decimal_as_string {
    ($row_ty:ty) => {
        |row: &$row_ty, i: usize| {
            row.try_get::<rust_decimal::Decimal, _>(i)
                .map(|d| Value::from(d.to_string()))
                .ok()
        }
    };
}

row_to_json!(pg_row_to_json, sqlx::postgres::PgRow, decimal_as_string!(sqlx::postgres::PgRow));
row_to_json!(my_row_to_json, sqlx::mysql::MySqlRow, decimal_as_string!(sqlx::mysql::MySqlRow));
// SQLite 是动态类型,声明成 DECIMAL 的列实际存 REAL
row_to_json!(sq_row_to_json, sqlx::sqlite::SqliteRow, |row: &sqlx::sqlite::SqliteRow, i: usize| {
    row.try_get::<f64, _>(i).map(Value::from).ok()
});

#[cfg(test)]
mod tests {
    use super::{Kind, sql_string};

    #[test]
    fn identifier_quoting_differs_by_dialect() {
        assert_eq!(Kind::Postgres.quote("my table"), "\"my table\"");
        assert_eq!(Kind::Mysql.quote("my table"), "`my table`");
        // 引号注入必须被转义掉
        assert_eq!(Kind::Postgres.quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(Kind::Mysql.quote("a`b"), "`a``b`");
    }

    #[test]
    fn kind_inferred_from_dsn() {
        assert_eq!(Kind::from_dsn("mysql://u:p@h/db"), Kind::Mysql);
        assert_eq!(Kind::from_dsn("mariadb://u:p@h/db"), Kind::Mysql);
        assert_eq!(Kind::from_dsn("postgres://u:p@h/db"), Kind::Postgres);
    }

    #[test]
    fn sqlite_is_recognised_and_file_based() {
        assert_eq!(Kind::from_dsn("sqlite:///data/shop.db"), Kind::Sqlite);
        assert_eq!(Kind::from_dsn("/tmp/shop.sqlite"), Kind::Sqlite);
        assert!(Kind::Sqlite.is_file_based());
        assert!(!Kind::Postgres.is_file_based());
        // SQLite 和 Postgres 一样用双引号
        assert_eq!(Kind::Sqlite.quote("my table"), "\"my table\"");
    }

    #[test]
    fn default_ports_match_the_engines() {
        assert_eq!(Kind::Postgres.default_port(), 5432);
        assert_eq!(Kind::Mysql.default_port(), 3306);
    }

    #[test]
    fn string_literals_escape_quotes() {
        assert_eq!(sql_string("o'brien"), "'o''brien'");
    }
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;

    /// 真跑一个 SQLite 文件:聚合列(SUM/COUNT)在 SQLite 里没有声明类型,
    /// 早期实现会把它们转成 null——模型于是报告「数据缺失」,是最坏的一种错。
    #[tokio::test]
    async fn aggregates_survive_the_json_roundtrip() {
        let path = std::env::temp_dir().join(format!("di-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("建库");
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, fee DECIMAL(10,2), tag TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t (fee, tag) VALUES (10.5,'a'),(20.25,'b'),(NULL,'c')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let p = Pool::connect(&format!("sqlite://{}", path.display()), 1)
            .await
            .expect("连接");
        let rows = p
            .query_json("SELECT count(*) AS n, SUM(fee) AS total, MAX(tag) AS last FROM t")
            .await
            .expect("查询");
        let r = &rows[0];
        assert_eq!(json_as_f64(r.get("n").unwrap()), Some(3.0), "count 丢了: {r}");
        assert_eq!(json_as_f64(r.get("total").unwrap()), Some(30.75), "SUM 丢了: {r}");
        assert_eq!(r.get("last").unwrap().as_str(), Some("c"), "文本丢了: {r}");

        // 明确的 NULL 仍要是 null,不能变成别的
        let rows = p.query_json("SELECT fee FROM t WHERE tag='c'").await.unwrap();
        assert!(rows[0].get("fee").unwrap().is_null(), "NULL 应保持为 null");

        p.close().await;
        let _ = std::fs::remove_file(&path);
    }
}
