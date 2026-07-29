//! di-server —— 服务库:连上 Postgres,选任意一个库,用模型直接查库回答经营/战略问题。
//! 不为每个库预先建模;Agent 自己 list_tables / describe_table / run_sql。
//! 只读保护(仅 SELECT + READ ONLY 事务 + 行数上限)+ hash 链审计。
//! 模型可换:默认走网关的 gemini-3.6-flash-high;要数据完全不出机器就换本地 Ollama
//! (注意:小模型会在 SQL 里编造系数,本地部署建议用较大的模型或加治理层)。
pub mod auth;
pub mod branch;
pub mod config;
pub mod governed;
pub mod db;
pub mod dialect;
pub mod tools;
pub mod webui;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use db::DbHub;
use harness_hooks::HashChainSink;
use harness_models::ApiKind;
use harness_serve::{Actor, ChatService, CorsConfig, InMemorySessions, StaticTokenAuth, http};
use serde_json::json;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// 语义模型目录:先看可执行文件旁边(装到客户机器上是这种形态),再回落到
/// 当前目录的 `models/`(源码里跑)。默认值不能是某台机器上的绝对路径。
fn models_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("DI_MODELS") {
        return std::path::PathBuf::from(d);
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("models")));
    match beside {
        Some(d) if d.is_dir() => d,
        _ => std::path::PathBuf::from("models"),
    }
}

const CONSULT_PROMPT: &str = "你是资深企业战略顾问(麦肯锡式),连着客户公司的真实数据库(只读)。\
【两种查数模式,先判断当前库属于哪种】\
A. 治理模式(库已配语义模型):必须用 list_metrics 看有哪些已定义指标 → get_dimensions 看某指标能按什么维度拆 \
→ query_metric 取数(参数形如 metrics=[\"net_margin\"], group_by=[\"store_name\"])。\
这种库的 run_sql 会被拒绝,不要反复尝试。指标口径已固定、跨 grain 已编译正确、含权限与脱敏。\
若某个指标缺维度,先 get_dimensions 确认维度名,再换维度重试;确实不支持才说明。\
B. 直连模式(库无语义模型):用 list_tables 看表 → describe_table 看列 → run_sql 写只读 SQL 查数,可多次逐步深入。\
两种模式共同要求:\
3) 数字必须来自查询结果,绝不编造;不确定就再查一次;\
4) 用假设驱动、结构化(MECE)的方式分析。输出面向 CEO,像一页纸:\
【结论先行】一句话核心判断;【关键发现】每条都带具体数据;【建议】排序、可执行、尽量量化影响。\
用中文,简洁有力;必要时用 markdown 表格。除非被问,不用暴露 SQL 细节。\
重要:涉及金额换算(万/亿)时,一律在 SQL 里用 round(值/10000.0,1) 或 round(值/100000000.0,2) 直接算好再展示,\
绝不自己口算换算,避免数量级(10 倍)错误;展示时直接抄查询结果里的数。\
【严禁编造系数】SQL 里只允许对真实列做聚合(sum/avg/count)和单位换算,\
绝不允许乘以任何臆想的系数(如 *1.4、*0.8「估算」「假设」),也不允许把没查到的数当成已知;\
数据里没有的东西就明说「库中无此数据」。展示的每个数字必须能在某次查询结果里逐字找到。\
先用 describe_table 或 SELECT min/max 确认数据的真实时间范围,不要假设。\
适合可视化时,在表格后另起一个 ```chart 代码块输出 ECharts option JSON(只用查到的数)。";

/// 数 `metrics:` 段里的条目。缩进由生成器决定(实测 4 空格),所以按段落边界数,
/// 而不是假定某个缩进宽度。
fn count_metrics(yaml: &str) -> usize {
    let mut in_metrics = false;
    let mut n = 0;
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent == 0 {
            // 顶层键:进入或离开 metrics 段
            in_metrics = trimmed.starts_with("metrics:");
            continue;
        }
        if in_metrics && trimmed.starts_with("- name:") {
            n += 1;
        }
    }
    n
}

/// 用 DataIntelligence 的 introspect 自动起草当前库的语义模型,存为 models/<库>.yaml。
/// 建模从「手写」变成「自动草拟 + 人工补治理(RBAC/脱敏/跨 grain 指标)」。
async fn model_generate(
    State((hub, gov)): State<(Arc<DbHub>, Arc<governed::Governed>)>,
) -> Json<serde_json::Value> {
    let db = hub.current().await;
    let Ok(dsn) = hub.dsn_for_current().await else {
        return Json(json!({"ok": false, "error": "尚未连接数据库"}));
    };
    let out = gov.models_dir().join(format!("{db}.yaml"));
    if let Some(d) = out.parent() { let _ = std::fs::create_dir_all(d); }
    let r = tokio::process::Command::new(gov.di_bin())
        .args(["model", "gen", "-dsn", &dsn, "-out", &out.to_string_lossy()])
        .output()
        .await;
    match r {
        Ok(o) if o.status.success() && out.is_file() => {
            let text = std::fs::read_to_string(&out).unwrap_or_default();
            let metrics = count_metrics(&text);
            Json(json!({"ok": true, "database": db, "path": out.to_string_lossy(),
                        "metrics": metrics, "governed": true}))
        }
        Ok(o) => Json(json!({"ok": false,
            "error": String::from_utf8_lossy(&o.stderr).chars().take(400).collect::<String>()})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(serde::Deserialize)]
struct BranchReq { name: String, #[serde(default)] tables: Option<Vec<String>> }

fn br_result(r: anyhow::Result<serde_json::Value>) -> Json<serde_json::Value> {
    Json(match r { Ok(v) => v, Err(e) => json!({"ok": false, "error": e.to_string()}) })
}

async fn branch_create(State(hub): State<Arc<DbHub>>, Json(b): Json<BranchReq>) -> Json<serde_json::Value> {
    br_result(branch::create(&hub, &b.name, b.tables).await)
}
async fn branch_diff_http(
    State(hub): State<Arc<DbHub>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    br_result(branch::diff(&hub, q.get("name").map(String::as_str).unwrap_or("")).await)
}
// 合并会写生产,只允许人工触发(模型没有这个工具)
async fn branch_promote(State(hub): State<Arc<DbHub>>, Json(b): Json<BranchReq>) -> Json<serde_json::Value> {
    br_result(branch::promote(&hub, &b.name).await)
}
async fn branch_discard(State(hub): State<Arc<DbHub>>, Json(b): Json<BranchReq>) -> Json<serde_json::Value> {
    br_result(branch::discard(&hub, &b.name).await)
}

// ── 设置(一次性,给 IT/实施):分字段填,不必认识「连接串」 ──
async fn setup_status(State(hub): State<Arc<DbHub>>) -> Json<serde_json::Value> {
    let saved = config::load();
    Json(json!({
        "configured": hub.is_configured().await,
        "current": hub.current().await,
        "saved": saved.map(|c| json!({
            "kind": c.kind, "host": c.host, "port": c.port, "user": c.user,
            "database": c.database, "ssl": c.ssl, "summary": c.summary(),
        })),
    }))
}

/// 试连:报表数量,让人一眼看出连对没有。不保存。
async fn setup_test(Json(c): Json<config::Connection>) -> Json<serde_json::Value> {
    match dialect::Pool::connect(&c.dsn(), 1).await {
        Ok(pool) => {
            let tables = pool.list_tables().await.map(|t| t.len()).unwrap_or(0);
            pool.close().await;
            Json(json!({"ok": true, "tables": tables,
                        "engine": pool.kind().label(), "summary": c.summary()}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn setup_save(State(hub): State<Arc<DbHub>>, Json(c): Json<config::Connection>) -> Json<serde_json::Value> {
    if let Err(e) = hub.configure(&c.dsn()).await {
        return Json(json!({"ok": false, "error": e.to_string()}));
    }
    match config::save(&c) {
        Ok(()) => Json(json!({"ok": true, "summary": c.summary()})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn evidence(
    State(log): State<Arc<tools::ExecLog>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let since = q.get("since").and_then(|s| s.parse::<u128>().ok()).unwrap_or(0);
    Json(json!({ "queries": log.since(since) }))
}

/// 列库,并标出哪些已配语义模型(前端据此显示「治理」标记)。
async fn list_dbs(
    State((hub, gov)): State<(Arc<DbHub>, Arc<governed::Governed>)>,
) -> Json<serde_json::Value> {
    let mut dbs = hub.list_databases().await.unwrap_or_default();
    for d in dbs.iter_mut() {
        let name = d.get("name").and_then(|v| v.as_str()).map(str::to_string);
        if let (Some(o), Some(n)) = (d.as_object_mut(), name) {
            o.insert("governed".into(), json!(gov.is_governed(&n)));
        }
    }
    let cur = hub.current().await;
    Json(json!({
        "current": cur,
        "governed": gov.is_governed(&cur),
        "databases": dbs,
    }))
}

/// 切库。返回该库是否走治理模式——由服务端说了算,前端不必自己推断(否则容易用到过期状态)。
async fn use_db(
    State((hub, gov)): State<(Arc<DbHub>, Arc<governed::Governed>)>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let Some(name) = body.get("db").and_then(|v| v.as_str()) else {
        return Json(json!({"ok": false, "error": "缺少 db 参数"}));
    };
    match hub.switch(name).await {
        Ok(()) => Json(json!({"ok": true, "current": name, "governed": gov.is_governed(name)})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

/// 启动服务并阻塞运行。二进制和 Tauri 壳共用同一份逻辑。
pub async fn run() -> anyhow::Result<()> {
    let dsn = config::startup_dsn();
    let model_name = env_or("LLM_MODEL", "gemini-3.6-flash-high");
    let base = env_or("LLM_BASE", "https://cpa.superleo.app/v1");
    let key = env_or("LLM_KEY", "ollama");
    let port = env_or("PORT", "43200");
    let audit_path = env_or("AUDIT", "/tmp/di-server-audit.jsonl");

    println!("== di-server · AI 战略顾问 ==");
    let hub = DbHub::empty();
    match &dsn {
        Some(d) => match hub.configure(d).await {
            Ok(()) => {
                let n = hub.list_databases().await.unwrap_or_default().len();
                println!("[db] 已连接 · 当前库 {} · 可选 {n} 个库", hub.current().await);
            }
            Err(e) => println!("[db] 连接失败({e}),请在页面「设置」里重新配置"),
        },
        None => println!("[db] 尚未配置数据库——打开页面按提示填写即可"),
    }

    let model = ApiKind::OpenAI.build(base.clone(), model_name.clone(), key);
    println!("[llm] 模型: {model_name} @ {base}");

    let (token, token_src) = auth::resolve_token();

    // 治理模式:models/<库>.yaml 存在就用 DataIntelligence 的固定口径查数
    // DI_BIN 默认就叫 `di`,由 PATH 解析——stdio 模式才需要它,HTTP 端点模式不需要。
    let gov = governed::Governed::new(
        env_or("DI_BIN", "di"),
        models_dir(),
        env_or("DI_ROLE", "finance"),
    );
    println!("[治理] 已建模的库: {:?}", gov.governed_databases());

    // 查询日志:回答里的每个数字都能追到这条真实执行过的 SQL
    let exec_log = Arc::new(tools::ExecLog::default());

    let mut svc = ChatService::new(
        model,
        Arc::new(StaticTokenAuth::new().with_token(token.clone(), Actor::new("consultant"))),
        Arc::new(InMemorySessions::new()),
        std::env::temp_dir().join("di-server-ws"),
    )
    .with_audit(Arc::new(HashChainSink::new(&audit_path)?))
    .with_instruction(CONSULT_PROMPT)
    .with_max_iters(16)
    .with_tool(Arc::new(tools::ListTables::new(hub.clone(), gov.clone())))
    .with_tool(Arc::new(tools::DescribeTable::new(hub.clone(), gov.clone())))
    .with_tool(Arc::new(tools::RunSql::new(hub.clone(), exec_log.clone(), gov.clone())))
    .with_tool(Arc::new(tools::BranchDiff::new(hub.clone())));
    for t in governed::GovernedProxy::all(gov.clone(), hub.clone()) {
        svc = svc.with_tool(t);
    }

    // 选库接口(带 CORS),与 harness-serve 的 /chat 路由合并,最后回落到静态 UI。
    let db_routes = Router::new()
        .route("/branch", post(branch_create))
        .route("/branch/diff", get(branch_diff_http))
        .route("/branch/promote", post(branch_promote))
        .route("/branch/discard", post(branch_discard))
        .route("/setup", get(setup_status))
        .route("/setup/save", post(setup_save))
        .with_state(hub.clone())
        .merge(
            Router::new()
                .route("/databases", get(list_dbs))
                .route("/use", post(use_db))
                .route("/model/generate", post(model_generate))
                .with_state((hub.clone(), gov.clone())),
        )
        .merge(Router::new().route("/setup/test", post(setup_test)))
        .merge(
            Router::new()
                .route("/evidence", get(evidence))
                .with_state(exec_log.clone()),
        )
        .layer(axum::middleware::from_fn(auth::guard))
        .layer(axum::Extension(auth::ExpectedToken(token.clone())))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    let base = http::router_with_cors(Arc::new(svc), &CorsConfig::permissive()).merge(db_routes);
    // 默认用编进二进制的界面;设了 WEB_DIR 就改用磁盘上的(改前端不必重编 Rust)
    let app = match std::env::var("WEB_DIR") {
        Ok(dir) if !dir.trim().is_empty() => base.fallback_service(ServeDir::new(dir)),
        _ => base
            .route("/", get(webui::index))
            .route("/{*path}", get(webui::asset)),
    };

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("[di-server] up on http://{addr}");
    println!("  GET /databases · POST /use {{\"db\":\"...\"}} · POST /chat/stream");
    println!("  audit → {audit_path}");
    println!("\n  访问令牌({token_src}): {token}");
    println!("  打开: http://localhost:{port}/?t={token}");
    println!("  令牌文件: {}\n", auth::config_path().display());
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::count_metrics;

    #[test]
    fn counts_only_entries_in_the_metrics_section() {
        // 生成器用 4 空格缩进,且 entities/dimensions 里也有 `- name:`
        let yaml = "entities:\n    - name: sale\n      table: sales\n\
                    dimensions:\n    - name: sale_sold_at\n      type: time\n\
                    metrics:\n    - name: sale_count\n      agg: count\n    - name: amount_sum\n      agg: sum\n";
        assert_eq!(count_metrics(yaml), 2);
    }

    #[test]
    fn counts_two_space_indentation_too() {
        let yaml = "metrics:\n  - name: a\n  - name: b\n  - name: c\n";
        assert_eq!(count_metrics(yaml), 3);
    }

    #[test]
    fn empty_when_no_metrics_section() {
        assert_eq!(count_metrics("entities:\n    - name: sale\n"), 0);
    }
}
