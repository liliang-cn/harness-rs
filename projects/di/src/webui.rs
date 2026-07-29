//! 内嵌前端:界面编译进二进制,部署就只有一个文件——不需要 WEB_DIR、不需要跟着拷 web/。
//!
//! 开发时想改前端不必重编 Rust:设 `WEB_DIR` 指向 `ui` 的构建产物即可覆盖内嵌版本。
use axum::body::Body;
use axum::extract::Path as AxPath;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};

static WEB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web");

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn serve(path: &str) -> Response {
    // 单页应用:静态文件命中就返回,其余一律回 index.html
    let file = WEB.get_file(path).or_else(|| WEB.get_file("index.html"));
    match file {
        Some(f) => {
            let ct = content_type(if WEB.get_file(path).is_some() { path } else { "index.html" });
            let mut res = Response::new(Body::from(f.contents().to_vec()));
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            // 带 hash 的资源可长期缓存;index.html 不缓存,免得升级后拿到旧壳
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            res
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

pub async fn index() -> Response {
    serve("index.html")
}

pub async fn asset(AxPath(path): AxPath<String>) -> Response {
    serve(path.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::{WEB, content_type};

    #[test]
    fn ui_is_embedded() {
        assert!(WEB.get_file("index.html").is_some(), "index.html 必须编进二进制");
        let n = WEB.files().count() + WEB.dirs().map(|d| d.files().count()).sum::<usize>();
        assert!(n >= 2, "至少应有 index.html 和一个资源文件,实际 {n}");
    }

    #[test]
    fn content_types_cover_the_bundle() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("assets/index-abc.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("assets/index-abc.css"), "text/css; charset=utf-8");
    }
}
