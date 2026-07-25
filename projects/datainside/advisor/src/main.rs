//! advisor —— 装上、连一个只读数据库、用本地模型直接查库回答经营/战略问题的 AI 顾问。
//! 数据不出机器(本地 Ollama 模型),只读保护 + hash 链审计。无需为每个库预先建模:
//! Agent 自己 list_tables / describe_table / run_sql,像顾问一样查数、给结论。
mod tools;

use harness_hooks::HashChainSink;
use harness_models::ApiKind;
use harness_serve::{ChatService, CorsConfig, InMemorySessions, OpenAuth, http};
use sqlx::{Row, postgres::PgPoolOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::services::ServeDir;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

const CONSULT_PROMPT: &str = "你是资深企业战略顾问(麦肯锡式),连着客户公司的真实数据库(只读)。回答经营/战略问题时:\
1) 先用 list_tables 看有哪些表,再用 describe_table 看关键表的列名与口径;\
2) 用 run_sql 写只读 SQL 查真实数字支撑每一个判断——先看总量、再拆维度、找异常,可多次逐步深入;\
3) 数字必须来自查询结果,绝不编造;不确定就再查一次;\
4) 用假设驱动、结构化(MECE)的方式分析。输出面向 CEO,像一页纸:\
【结论先行】一句话核心判断;【关键发现】每条都带具体数据;【建议】排序、可执行、尽量量化影响。\
用中文,简洁有力;必要时用 markdown 表格。除非被问,不用暴露 SQL 细节。\
重要:涉及金额换算(万/亿)时,一律在 SQL 里用 round(值/10000.0,1) 或 round(值/100000000.0,2) 直接算好再展示,\
绝不自己口算换算,避免数量级(10 倍)错误;展示时直接抄查询结果里的数。";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dsn = env_or(
        "ADVISOR_DSN",
        "postgres://reformd:reformd@localhost:47615/conglomerate?sslmode=disable",
    );
    let model_name = env_or("LLM_MODEL", "ornith:latest");
    let base = env_or("LLM_BASE", "http://localhost:11434/v1");
    let key = env_or("LLM_KEY", "ollama");
    let port = env_or("PORT", "43200");
    let audit_path = env_or("AUDIT", "/tmp/advisor-audit.jsonl");
    let web_dir = std::env::var_os("WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"));
    let query_timeout_secs = env_or("QUERY_TIMEOUT_SECS", "30").parse::<u64>()?;

    println!("== AI 战略顾问 (advisor) ==");
    println!(
        "[db] 只读连接: {}",
        dsn.split('@').next_back().unwrap_or(&dsn)
    );
    let pool = Arc::new(
        PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                Box::pin(async move {
                    sqlx::query("SET default_transaction_read_only = on")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                        .bind(format!("{query_timeout_secs}s"))
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&dsn)
            .await?,
    );
    let db_settings = sqlx::query(
        "SELECT current_setting('default_transaction_read_only') AS read_only,
                current_setting('statement_timeout') AS statement_timeout",
    )
    .fetch_one(&*pool)
    .await?;
    println!(
        "[db] 连接成功 (read_only={}, statement_timeout={})",
        db_settings.get::<String, _>("read_only"),
        db_settings.get::<String, _>("statement_timeout")
    );

    let model = ApiKind::OpenAI.build(base.clone(), model_name.clone(), key);
    println!("[llm] 模型: {model_name} @ {base}");

    let svc = ChatService::new(
        model,
        Arc::new(OpenAuth::new("consultant")),
        Arc::new(InMemorySessions::new()),
        std::env::temp_dir().join("advisor-ws"),
    )
    .with_audit(Arc::new(HashChainSink::new(&audit_path)?))
    .with_instruction(CONSULT_PROMPT)
    .with_max_iters(16)
    .with_tool(Arc::new(tools::ListTables::new(pool.clone())))
    .with_tool(Arc::new(tools::DescribeTable::new(pool.clone())))
    .with_tool(Arc::new(tools::RunSql::new(pool.clone())));

    let app = http::router_with_cors(Arc::new(svc), &CorsConfig::permissive())
        .fallback_service(ServeDir::new(&web_dir).append_index_html_on_directories(true));

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("[advisor] up on http://{addr}  (UI + POST /chat/stream)");
    println!("  SQL timeout → {query_timeout_secs}s");
    println!("  audit → {audit_path}");
    axum::serve(listener, app).await?;
    Ok(())
}
