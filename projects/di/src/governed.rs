//! 治理模式:某个库配了语义模型(`models/<库名>.yaml`)时,改用 DataIntelligence 的
//! 治理工具查数,而不是让模型自己写 SQL。
//!
//! 为什么需要:直连 SQL 零建模、当天可用,但正确性依赖模型——实测弱模型会在 SQL 里
//! 编造系数(`SUM(amount)*1.4`)。治理层把口径固定在语义模型里:模型只能从声明好的
//! 指标里选,跨 grain 的比率(库存周转、净利、入住率)由 DI 逐 grain 编译,
//! 结构上不可能算错,也不可能越权——RBAC 与脱敏都在语义层。
//!
//! **两种接入 DI 的方式**(DI 的一个进程只服务一个模型+一个库,这是它的配置结构决定的):
//! - **HTTP**(生产):DI 作为独立服务跑 `di mcp -http :端口`,本进程只做客户端。
//!   不需要本机有 DI 二进制,也不用管子进程死活,DI 可独立部署与扩容。
//!   端点写在 `models/endpoints.json`。
//! - **stdio**(单机/开发):本进程按需拉起 `di mcp` 子进程。要求本机装了 DI。
use harness_core::Tool;
use harness_mcp_client::McpClient;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// DI 只暴露这几个只读工具——没有 run_sql,模型碰不到原始 SQL。
const GOVERNED_TOOLS: &[&str] = &["list_metrics", "get_dimensions", "query_metric"];

/// `models/endpoints.json` 里一个库对应的 DI 服务。
#[derive(Clone, serde::Deserialize)]
pub struct Endpoint {
    pub url: String,
    /// bearer 令牌;留空则用 `DI_MCP_TOKEN`。DI 的角色由令牌决定。
    #[serde(default)]
    pub token: String,
}

pub struct Governed {
    di_bin: String,
    models_dir: PathBuf,
    role: String,
    /// 库名 → DI 的 HTTP 端点(有则走 HTTP,没有则回落到 stdio 子进程)
    endpoints: HashMap<String, Endpoint>,
    /// 每个库一套治理工具,按需建立并缓存。
    cache: Mutex<HashMap<String, Arc<Vec<Arc<dyn Tool>>>>>,
    /// 持有 MCP 会话:stdio 模式下子进程活着工具才有效。
    sessions: Mutex<Vec<McpClient>>,
}

impl Governed {
    pub fn new(di_bin: String, models_dir: PathBuf, role: String) -> Arc<Self> {
        let endpoints = load_endpoints(&models_dir);
        if !endpoints.is_empty() {
            let mut names: Vec<&String> = endpoints.keys().collect();
            names.sort();
            println!("[治理] HTTP 端点: {names:?}");
        }
        Arc::new(Self {
            di_bin,
            models_dir,
            role,
            endpoints,
            cache: Mutex::new(HashMap::new()),
            sessions: Mutex::new(Vec::new()),
        })
    }

    /// 某个库的语义模型路径(存在才算「已治理」)。
    pub fn model_path(&self, db: &str) -> Option<PathBuf> {
        let p = self.models_dir.join(format!("{db}.yaml"));
        p.is_file().then_some(p)
    }

    pub fn models_dir(&self) -> &Path {
        &self.models_dir
    }
    pub fn di_bin(&self) -> &str {
        &self.di_bin
    }

    /// 配了 HTTP 端点、或本地有语义模型,都算已治理。
    pub fn is_governed(&self, db: &str) -> bool {
        self.endpoints.contains_key(db) || self.model_path(db).is_some()
    }

    /// 列出所有走治理模式的库名。
    pub fn governed_databases(&self) -> Vec<String> {
        let mut out: Vec<String> = self.endpoints.keys().cloned().collect();
        if let Ok(rd) = std::fs::read_dir(&self.models_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("yaml")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                    && !out.iter().any(|x| x == stem)
                {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        out
    }

    /// 取某库的治理工具:优先 HTTP 端点,否则拉起 stdio 子进程。
    pub async fn tools_for(&self, db: &str, dsn: &str) -> anyhow::Result<Arc<Vec<Arc<dyn Tool>>>> {
        if let Some(t) = self.cache.lock().await.get(db) {
            return Ok(t.clone());
        }
        let client = match self.endpoints.get(db) {
            Some(ep) => connect_http(ep).await?,
            None => {
                let model = self
                    .model_path(db)
                    .ok_or_else(|| anyhow::anyhow!("库 {db} 没有语义模型,也没有配 DI 端点"))?;
                connect_stdio(&self.di_bin, &model, dsn, &self.role).await?
            }
        };
        let tools: Arc<Vec<Arc<dyn Tool>>> =
            Arc::new(client.tools_with_read_only(GOVERNED_TOOLS).into_iter().collect());
        self.sessions.lock().await.push(client); // 会话活着工具才有效
        self.cache.lock().await.insert(db.to_string(), tools.clone());
        Ok(tools)
    }
}

/// 读 `models/endpoints.json`:`{"库名": {"url": "http://host:port", "token": "…"}}`。
/// 文件不存在就是纯 stdio 模式,不是错误。
fn load_endpoints(models_dir: &Path) -> HashMap<String, Endpoint> {
    let path = models_dir.join("endpoints.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, Endpoint>>(&text) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[治理] {} 解析失败,忽略: {e}", path.display());
            HashMap::new()
        }
    }
}

/// HTTP 接入:DI 独立跑,本进程只是客户端。
/// 用注入 client 的方式带上 bearer 头——DI 的角色由令牌决定,不再由命令行 `-role` 指定。
async fn connect_http(ep: &Endpoint) -> anyhow::Result<McpClient> {
    let token = if ep.token.is_empty() {
        std::env::var("DI_MCP_TOKEN").unwrap_or_default()
    } else {
        ep.token.clone()
    };
    let mut headers = harness_mcp_client::reqwest::header::HeaderMap::new();
    if !token.is_empty() {
        headers.insert(
            harness_mcp_client::reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse()?,
        );
    }
    let client = harness_mcp_client::reqwest::Client::builder()
        .default_headers(headers)
        // 端点来自本机配置文件而非模型输出,但仍禁跳转:避免被重定向到内网地址
        .redirect(harness_mcp_client::reqwest::redirect::Policy::none())
        .build()?;
    McpClient::connect_http_with_client(&ep.url, client).await
}

/// stdio 接入:按需拉起本机的 DI 子进程(单机/开发用)。
async fn connect_stdio(
    di_bin: &str,
    model: &Path,
    dsn: &str,
    role: &str,
) -> anyhow::Result<McpClient> {
    let model = model.to_string_lossy().to_string();
    McpClient::connect_stdio(
        di_bin,
        &["mcp", "-model", &model, "-dsn", dsn, "-role", role],
    )
    .await
}

// ── 代理工具 ──────────────────────────────────────────────────────────────
// 工具在启动时注册一次,但「走治理还是走直连」要按库切换。所以注册的是代理:
// 调用时才根据当前库解析到对应的 DI 会话;库没配语义模型就如实说明。
use async_trait::async_trait;
use harness_core::{ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};

pub struct GovernedProxy {
    inner_name: String,
    schema: ToolSchema,
    gov: Arc<Governed>,
    hub: Arc<crate::db::DbHub>,
}

impl GovernedProxy {
    pub fn new(
        inner_name: &str,
        description: &str,
        input: Value,
        gov: Arc<Governed>,
        hub: Arc<crate::db::DbHub>,
    ) -> Self {
        Self {
            inner_name: inner_name.to_string(),
            schema: ToolSchema {
                name: inner_name.to_string(),
                description: description.to_string(),
                input,
            },
            gov,
            hub,
        }
    }

    /// 三个治理工具的代理。参数 schema 交给 DI 的真实工具校验,这里保持开放。
    pub fn all(gov: Arc<Governed>, hub: Arc<crate::db::DbHub>) -> Vec<Arc<dyn Tool>> {
        let open = || json!({"type":"object","additionalProperties":true});
        vec![
            Arc::new(Self::new(
                "list_metrics",
                "列出当前库语义模型里已定义的指标(口径已固定、可直接使用)。治理模式下先用它。",
                open(),
                gov.clone(),
                hub.clone(),
            )) as Arc<dyn Tool>,
            Arc::new(Self::new(
                "get_dimensions",
                "列出某个指标可用的维度。治理模式下用它确认能按什么拆分。",
                open(),
                gov.clone(),
                hub.clone(),
            )),
            Arc::new(Self::new(
                "query_metric",
                "按已定义的指标+维度查数(治理口径,跨 grain 自动编译正确,含 RBAC 与脱敏)。治理模式下用它代替写 SQL。",
                open(),
                gov,
                hub,
            )),
        ]
    }
}

#[async_trait]
impl Tool for GovernedProxy {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }

    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        let db = self.hub.current().await;
        if !self.gov.is_governed(&db) {
            return Ok(ToolResult {
                ok: false,
                content: json!({"error": format!("库 {db} 未配置语义模型,请改用 run_sql 直接查询")}),
                trace: None,
            });
        }
        let dsn = self
            .hub
            .dsn_for_current()
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        let tools = self
            .gov
            .tools_for(&db, &dsn)
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        let t = tools
            .iter()
            .find(|t| t.name() == self.inner_name)
            .ok_or_else(|| ToolError::NotFound { name: self.inner_name.clone() })?;
        t.invoke(args, world).await
    }
}

#[cfg(test)]
mod tests {
    use super::load_endpoints;

    #[test]
    fn endpoints_file_is_optional_and_malformed_json_is_ignored() {
        let dir = std::env::temp_dir().join(format!("di-gov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 没有文件 → 纯 stdio 模式,不该报错
        assert!(load_endpoints(&dir).is_empty());

        // 坏 JSON → 忽略而不是崩(否则一个手滑的配置会让整个服务起不来)
        std::fs::write(dir.join("endpoints.json"), "{ not json").unwrap();
        assert!(load_endpoints(&dir).is_empty());

        std::fs::write(
            dir.join("endpoints.json"),
            r#"{"shop":{"url":"http://localhost:41955","token":"finance-token"}}"#,
        )
        .unwrap();
        let m = load_endpoints(&dir);
        assert_eq!(m["shop"].url, "http://localhost:41955");
        assert_eq!(m["shop"].token, "finance-token");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
