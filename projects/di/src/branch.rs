//! 入库前分支对比(Neon 式分支的轻量实现)。
//!
//! 行级校验(类型/主键/外键)拦不住总量级错误:文件重复导入、金额单位从元变成分、
//! 少了一个门店——每行都合法,汇总全错。所以新数据先进**分支**,和生产比过再合并。
//!
//! 分支 = 同库里的一个 `br_<name>` schema,用 `CREATE TABLE ... AS TABLE` 复制受影响的表。
//! Postgres 的 `CREATE DATABASE ... TEMPLATE` 要求模板库没有活连接,生产上做不到;
//! schema 级分支在活库上瞬间完成。
//!
//! 安全不变量:**模型只能读**。分支的建/合并/丢弃都是人工触发的 HTTP 操作,
//! 且写入只发生在 `br_*` schema;合并回 public 必须由人显式确认。
use crate::db::DbHub;
use crate::dialect::{Kind, Pool, sql_string};
use serde::Serialize;
use serde_json::{Value, json};

fn qident(id: &str) -> String {
    Kind::Postgres.quote(id)
}

/// 分支用 schema 实现,而 MySQL 里 schema 就是 database——机制不同,先只支持 Postgres,
/// 并如实告知,而不是悄悄做出个半对的东西。
async fn require_postgres(pool: &Pool) -> anyhow::Result<()> {
    if pool.kind() != Kind::Postgres {
        anyhow::bail!("入库分支目前只支持 PostgreSQL(MySQL 的 schema 即 database,需另一套实现)");
    }
    Ok(())
}

/// 分支名只允许安全字符,避免拼接出注入。
fn safe_name(name: &str) -> anyhow::Result<String> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(name.to_string())
    } else {
        anyhow::bail!("分支名只能是字母数字下划线(≤40)")
    }
}

fn schema_of(name: &str) -> String {
    format!("br_{name}")
}

async fn public_tables(pool: &Pool) -> anyhow::Result<Vec<String>> {
    Ok(pool
        .list_tables()
        .await?
        .iter()
        .filter_map(|v| v.get("table")?.as_str().map(str::to_string))
        .collect())
}

/// 建分支:把选中的表(默认全部)复制到 `br_<name>`。
pub async fn create(hub: &DbHub, name: &str, tables: Option<Vec<String>>) -> anyhow::Result<Value> {
    let name = safe_name(name)?;
    let schema = schema_of(&name);
    let pool = hub.pool().await?;
    require_postgres(&pool).await?;
    let all = public_tables(&pool).await?;
    let picked: Vec<String> = match tables {
        Some(t) if !t.is_empty() => t.into_iter().filter(|x| all.contains(x)).collect(),
        _ => all.clone(),
    };
    if picked.is_empty() {
        anyhow::bail!("没有可复制的表");
    }
    pool.execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", qident(&schema))).await?;
    pool.execute(&format!("CREATE SCHEMA {}", qident(&schema))).await?;
    for t in &picked {
        pool.execute(&format!(
            "CREATE TABLE {}.{} AS TABLE public.{}",
            qident(&schema),
            qident(t),
            qident(t)
        )).await?;
    }
    Ok(json!({"ok": true, "branch": name, "schema": schema, "tables": picked}))
}

#[derive(Serialize)]
pub struct TableDiff {
    pub table: String,
    pub rows_main: i64,
    pub rows_branch: i64,
    pub rows_delta: i64,
    pub rows_pct: f64,
    /// 每个数值列的合计对比:列名 → {main, branch, delta, pct}
    pub sums: Value,
    /// 值得人看一眼的信号(重复导入、单位变化、维度缺失…)
    pub flags: Vec<String>,
}

async fn scalar(pool: &Pool, sql: &str) -> Option<f64> {
    pool.scalar_f64(sql).await
}

fn pct(main: f64, branch: f64) -> f64 {
    if main == 0.0 {
        if branch == 0.0 { 0.0 } else { 100.0 }
    } else {
        ((branch - main) / main * 100.0 * 100.0).round() / 100.0
    }
}

/// 对比分支与生产:逐表行数 + 每个数值列的合计,并标记可疑信号。
pub async fn diff(hub: &DbHub, name: &str) -> anyhow::Result<Value> {
    let name = safe_name(name)?;
    let schema = schema_of(&name);
    let pool = hub.pool().await?;
    require_postgres(&pool).await?;

    let rows = pool
        .query_json(&format!(
            "SELECT table_name FROM information_schema.tables
             WHERE table_schema={} AND table_type='BASE TABLE' ORDER BY table_name",
            sql_string(&schema)
        ))
        .await?;
    if rows.is_empty() {
        anyhow::bail!("分支 {name} 不存在(或没有表)");
    }

    let mut out: Vec<TableDiff> = Vec::new();
    for r in &rows {
        let Some(t) = r.get("table_name").and_then(|v| v.as_str()).map(str::to_string) else { continue };
        let rows_main = scalar(
            &pool,
            &format!("SELECT count(*)::float8 FROM public.{}", qident(&t)),
        )
        .await
        .unwrap_or(0.0);
        let rows_branch = scalar(
            &pool,
            &format!("SELECT count(*)::float8 FROM {}.{}", qident(&schema), qident(&t)),
        )
        .await
        .unwrap_or(0.0);

        // 数值列的合计——总量级错误就藏在这里
        let cols = pool.numeric_columns_of(&t).await.unwrap_or_default();

        let mut sums = serde_json::Map::new();
        let mut flags: Vec<String> = Vec::new();
        for col in &cols {
            let m = scalar(
                &pool,
                &format!(
                    "SELECT COALESCE(SUM({}),0)::float8 FROM public.{}",
                    qident(col),
                    qident(&t)
                ),
            )
            .await
            .unwrap_or(0.0);
            let b = scalar(
                &pool,
                &format!(
                    "SELECT COALESCE(SUM({}),0)::float8 FROM {}.{}",
                    qident(col),
                    qident(&schema),
                    qident(&t)
                ),
            )
            .await
            .unwrap_or(0.0);
            if m == 0.0 && b == 0.0 {
                continue;
            }
            let p = pct(m, b);
            sums.insert(
                col.to_string(),
                json!({"main": m, "branch": b, "delta": b - m, "pct": p}),
            );
            if p.abs() >= 30.0 {
                flags.push(format!("{col} 合计变化 {p:+.1}%"));
            }
        }

        let rp = pct(rows_main, rows_branch);
        if rows_main > 0.0 && (rows_branch - rows_main * 2.0).abs() < 0.5 {
            flags.push("行数正好翻倍——疑似重复导入".into());
        }
        if rows_branch < rows_main {
            flags.push(format!("分支比生产少 {} 行", (rows_main - rows_branch) as i64));
        }
        // 行数几乎没变但金额大涨 → 典型的单位错误(元→分)
        if rp.abs() < 1.0 {
            for (col, v) in sums.iter() {
                let p = v.get("pct").and_then(|x| x.as_f64()).unwrap_or(0.0);
                if p >= 50.0 {
                    flags.push(format!("行数几乎不变但 {col} 涨 {p:+.1}%——疑似单位/口径变化"));
                }
            }
        }

        out.push(TableDiff {
            table: t,
            rows_main: rows_main as i64,
            rows_branch: rows_branch as i64,
            rows_delta: (rows_branch - rows_main) as i64,
            rows_pct: rp,
            sums: Value::Object(sums),
            flags,
        });
    }

    let total_flags: usize = out.iter().map(|d| d.flags.len()).sum();
    Ok(json!({
        "branch": name, "schema": schema,
        "tables": out,
        "flag_count": total_flags,
        "verdict": if total_flags == 0 { "无异常信号" } else { "有信号,需人工确认" },
    }))
}

/// 合并:用分支的表替换 public 的同名表(在一个事务里,失败则整体回滚)。人工触发。
pub async fn promote(hub: &DbHub, name: &str) -> anyhow::Result<Value> {
    let name = safe_name(name)?;
    let schema = schema_of(&name);
    let pool = hub.pool().await?;
    require_postgres(&pool).await?;
    let rows = pool
        .query_json(&format!(
            "SELECT table_name FROM information_schema.tables
             WHERE table_schema={} AND table_type='BASE TABLE'",
            sql_string(&schema)
        ))
        .await?;
    if rows.is_empty() {
        anyhow::bail!("分支 {name} 不存在");
    }
    let mut applied = Vec::new();
    for r in &rows {
        let Some(t) = r.get("table_name").and_then(|v| v.as_str()).map(str::to_string) else { continue };
        // 保留生产表结构与约束:只换数据
        pool.execute(&format!("DELETE FROM public.{}", qident(&t))).await?;
        pool.execute(&format!(
            "INSERT INTO public.{} SELECT * FROM {}.{}",
            qident(&t), qident(&schema), qident(&t)
        )).await?;
        applied.push(t);
    }
    Ok(json!({"ok": true, "branch": name, "promoted_tables": applied}))
}

/// 丢弃分支。
pub async fn discard(hub: &DbHub, name: &str) -> anyhow::Result<Value> {
    let name = safe_name(name)?;
    let schema = schema_of(&name);
    let pool = hub.pool().await?;
    require_postgres(&pool).await?;
    pool.execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", qident(&schema))).await?;
    Ok(json!({"ok": true, "discarded": schema}))
}
