//! 连接配置:分字段保存,用户不必认识「连接串」。
//!
//! 装上后程序照常启动(即使还没配过库),由设置页填主机/账号/密码 → 测试 → 保存。
//! 保存在 `~/.di-server/connection.json`,权限 0600(里面有密码)。
use crate::dialect::Kind;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    /// 数据库类型:postgres 或 mysql
    #[serde(default = "default_kind")]
    pub kind: Kind,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: String,
    pub database: String,
    /// 是否要求 TLS。内网库通常关掉。
    #[serde(default)]
    pub ssl: bool,
}

fn default_port() -> u16 {
    5432
}

fn default_kind() -> Kind {
    Kind::Postgres
}

impl Connection {
    /// 拼成 sqlx 用的 DSN。密码做百分号转义,免得特殊字符把 URL 拆坏。
    /// 两种数据库的 TLS 参数写法不同,由这里统一处理。
    pub fn dsn(&self) -> String {
        let auth = format!("{}:{}", enc(&self.user), enc(&self.password));
        match self.kind {
            Kind::Postgres => format!(
                "postgres://{auth}@{}:{}/{}?sslmode={}",
                self.host, self.port, self.database,
                if self.ssl { "require" } else { "disable" }
            ),
            Kind::Mysql => format!(
                "mysql://{auth}@{}:{}/{}{}",
                self.host, self.port, self.database,
                if self.ssl { "?ssl-mode=REQUIRED" } else { "" }
            ),
            // 文件库:没有主机/账号,`database` 字段存的是文件路径
            Kind::Sqlite => format!("sqlite://{}?mode=ro", self.database),
        }
    }

    /// 给界面看的简述,不含密码。
    pub fn summary(&self) -> String {
        match self.kind {
            Kind::Sqlite => self.database.clone(),
            _ => format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database),
        }
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn path() -> PathBuf {
    if let Ok(p) = std::env::var("DI_SERVER_CONNECTION") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".di-server").join("connection.json")
}

pub fn load() -> Option<Connection> {
    let text = std::fs::read_to_string(path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(conn: &Connection) -> anyhow::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, serde_json::to_vec_pretty(conn)?)?;
    // 文件里有数据库密码
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 启动时的连接来源:环境变量优先(给运维/容器),否则用设置页存的配置。
pub fn startup_dsn() -> Option<String> {
    if let Ok(dsn) = std::env::var("DI_SERVER_DSN") {
        if !dsn.trim().is_empty() {
            return Some(dsn);
        }
    }
    load().map(|c| c.dsn())
}

#[cfg(test)]
mod tests {
    use super::Connection;
    use crate::dialect::Kind;

    fn conn(password: &str) -> Connection {
        Connection {
            kind: Kind::Postgres,
            host: "db.internal".into(),
            port: 5432,
            user: "readonly".into(),
            password: password.into(),
            database: "erp_prod".into(),
            ssl: false,
        }
    }

    #[test]
    fn dsn_escapes_special_characters_in_password() {
        // 密码里的 @ / : 会把朴素拼接出来的 URL 拆坏
        let dsn = conn("p@ss:w/rd").dsn();
        assert!(dsn.contains("p%40ss%3Aw%2Frd"), "{dsn}");
        assert!(dsn.starts_with("postgres://readonly:"));
        assert!(dsn.ends_with("@db.internal:5432/erp_prod?sslmode=disable"));
    }

    #[test]
    fn summary_never_leaks_the_password() {
        assert_eq!(conn("secret").summary(), "readonly@db.internal:5432/erp_prod");
    }

    #[test]
    fn mysql_dsn_uses_its_own_scheme_and_tls_param() {
        let mut c = conn("p@ss");
        c.kind = Kind::Mysql;
        c.port = 3306;
        assert!(c.dsn().starts_with("mysql://readonly:p%40ss@"), "{}", c.dsn());
        c.ssl = true;
        assert!(c.dsn().ends_with("?ssl-mode=REQUIRED"), "{}", c.dsn());
    }

    #[test]
    fn sqlite_dsn_is_a_read_only_file_path() {
        let mut c = conn("");
        c.kind = Kind::Sqlite;
        c.database = "/data/shop.db".into();
        assert_eq!(c.dsn(), "sqlite:///data/shop.db?mode=ro");
        // 文件库的简述就是路径,不该出现空账号@空主机
        assert_eq!(c.summary(), "/data/shop.db");
    }

    #[test]
    fn ssl_flag_selects_sslmode() {
        let mut c = conn("x");
        c.ssl = true;
        assert!(c.dsn().ends_with("sslmode=require"));
    }
}
