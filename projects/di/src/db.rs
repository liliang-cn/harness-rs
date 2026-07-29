//! DbHub —— 一个 Postgres 实例上的多库接入:列出所有库、按需切换、连接池按库缓存。
//! 顾问因此变成「装上 → 选库 → 提问」,不需要为每个库预先建模或重启。
use crate::dialect::{Kind, Pool};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// 把 `postgres://…/db?params` 或 `mysql://…/db` 拆成 (前缀, 库名, 参数),
/// 以便换库时只替换库名。
fn split_dsn(dsn: &str) -> (String, String, String) {
    let scheme_end = dsn.find("://").map(|i| i + 3).unwrap_or(0);
    let rest = &dsn[scheme_end..];
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let (db, params) = match path.find('?') {
        Some(i) => (&path[..i], &path[i..]),
        None => (path, ""),
    };
    (
        format!("{}{}", &dsn[..scheme_end], authority),
        db.to_string(),
        params.to_string(),
    )
}

/// 当前指向的服务器与库。未配置时为 None——程序照常启动,由设置页配好再用。
struct Target {
    prefix: String,
    params: String,
    current: String,
}

pub struct DbHub {
    target: RwLock<Option<Target>>,
    pools: Mutex<HashMap<String, Arc<Pool>>>,
}

impl DbHub {
    /// 空 hub:还没配过数据库。
    pub fn empty() -> Arc<Self> {
        Arc::new(Self { target: RwLock::new(None), pools: Mutex::new(HashMap::new()) })
    }

    /// 用一个初始 DSN 建立 hub,并验证初始库可连。
    pub async fn connect(dsn: &str) -> anyhow::Result<Arc<Self>> {
        let hub = Self::empty();
        hub.configure(dsn).await?;
        Ok(hub)
    }

    /// 换一台服务器/一个库(设置页保存时调用)。验证能连上才生效。
    pub async fn configure(&self, dsn: &str) -> anyhow::Result<()> {
        let (prefix, db, params) = split_dsn(dsn);
        // 先试连,失败就不动现有配置
        let probe = Pool::connect(dsn, 1).await?;
        probe.close().await;
        *self.target.write().await = Some(Target { prefix, params, current: db });
        self.pools.lock().await.clear();
        Ok(())
    }

    pub async fn is_configured(&self) -> bool {
        self.target.read().await.is_some()
    }

    async fn dsn_for(&self, db: &str) -> anyhow::Result<String> {
        let guard = self.target.read().await;
        let t = guard.as_ref().ok_or_else(|| anyhow::anyhow!("尚未配置数据库,请先在设置里连接"))?;
        Ok(format!("{}/{}{}", t.prefix, db, t.params))
    }

    /// 当前库的完整 DSN(治理模式下要把它交给 DataIntelligence)。
    pub async fn dsn_for_current(&self) -> anyhow::Result<String> {
        let db = self.current().await;
        self.dsn_for(&db).await
    }

    pub async fn current(&self) -> String {
        self.target.read().await.as_ref().map(|t| t.current.clone()).unwrap_or_default()
    }

    /// 当前库的连接池(按库缓存,首次使用时建立)。
    pub async fn pool(&self) -> anyhow::Result<Arc<Pool>> {
        let db = self.current().await;
        self.pool_for(&db).await
    }

    pub async fn pool_for(&self, db: &str) -> anyhow::Result<Arc<Pool>> {
        let dsn = self.dsn_for(db).await?;
        let mut pools = self.pools.lock().await;
        if let Some(p) = pools.get(db) {
            return Ok(p.clone());
        }
        let pool = Arc::new(Pool::connect(&dsn, 4).await?);
        pools.insert(db.to_string(), pool.clone());
        Ok(pool)
    }

    /// 这台服务器上所有可选的业务库(方言差异交给 dialect 处理)。
    pub async fn list_databases(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        self.pool().await?.list_databases().await
    }

    /// 当前连接是哪种数据库(界面上显示 PostgreSQL / MySQL)。
    pub async fn kind(&self) -> Option<Kind> {
        let dsn = self.dsn_for_current().await.ok()?;
        Some(Kind::from_dsn(&dsn))
    }

    /// 切库:先验证能连上,再切换当前选择。
    pub async fn switch(&self, db: &str) -> anyhow::Result<()> {
        self.pool_for(db).await?;
        if let Some(t) = self.target.write().await.as_mut() {
            t.current = db.to_string();
        }
        Ok(())
    }
}
