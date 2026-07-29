//! 访问令牌:装上就能用,但默认不是敞开的。
//!
//! 取值顺序:`DI_SERVER_TOKEN` 环境变量 → 配置文件 → 首次运行随机生成并写入配置。
//! 生成式(Jupyter 那种)是为了让「装上输入 KEY 就能用」不牺牲安全:
//! 用户不配置也有一个真令牌,而不是零认证。
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use std::path::PathBuf;

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("DI_SERVER_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".di-server").join("token")
}

fn random_token() -> String {
    // 32 字节随机(/dev/urandom),十六进制;失败则退回时间+进程号,不静默用弱值。
    let mut bytes = [0u8; 24];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok()
    {
        return bytes.iter().map(|b| format!("{b:02x}")).collect();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{:x}", std::process::id())
}

/// 解析出本次运行使用的令牌,并说明它从哪来(供启动日志提示)。
pub fn resolve_token() -> (String, &'static str) {
    if let Ok(t) = std::env::var("DI_SERVER_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return (t, "环境变量 DI_SERVER_TOKEN");
        }
    }
    let path = config_path();
    if let Ok(t) = std::fs::read_to_string(&path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return (t, "配置文件");
        }
    }
    let t = random_token();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&path, &t).is_ok() {
        // 令牌等同密码,别让同机其他用户读到
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    (t, "首次运行已生成")
}

/// 保护需要授权的路由:`Authorization: Bearer <token>` 或 `?t=<token>`。
/// 静态页面和 /healthz 不走这里——不然用户连输入令牌的页面都打不开。
pub async fn guard(req: Request, next: Next) -> Result<Response, StatusCode> {
    let expected = req
        .extensions()
        .get::<ExpectedToken>()
        .map(|t| t.0.clone())
        .unwrap_or_default();
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let from_query = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("t=").map(|v| v.to_string()))
        })
        .unwrap_or_default();
    if constant_eq(bearer, &expected) || constant_eq(&from_query, &expected) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[derive(Clone)]
pub struct ExpectedToken(pub String);

/// 常数时间比较,避免按字符早退泄漏令牌长度/前缀。
fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::constant_eq;

    #[test]
    fn constant_eq_matches_only_identical_non_empty_tokens() {
        assert!(constant_eq("abc123", "abc123"));
        assert!(!constant_eq("abc123", "abc124"));
        assert!(!constant_eq("abc", "abc123"));
        assert!(!constant_eq("", ""), "空令牌不能通过");
    }
}
