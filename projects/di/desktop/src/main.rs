//! di-server 的桌面壳:双击图标就能用,不必开终端、不必复制带令牌的链接。
//!
//! 壳很薄——业务逻辑全在 `di_server` 库里,和服务器版跑的是同一份实现。
//! 这里只做三件事:后台起服务、等它就绪、把窗口指过去(URL 里带上本机令牌)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const PORT: &str = "43217"; // 桌面版用独立端口,免得和服务器版撞

/// 轮询直到 /healthz 有响应——窗口早于服务打开会白屏。
async fn wait_ready(port: &str) -> bool {
    let url = format!("http://127.0.0.1:{port}/healthz");
    for _ in 0..100 {
        if reqwest::get(&url).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn main() {
    // 桌面版只在本机自用:端口固定,令牌沿用 ~/.di-server/token(首次运行自动生成)
    if std::env::var("PORT").is_err() {
        unsafe { std::env::set_var("PORT", PORT) };
    }

    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // 和服务器版同一份 run(),不是复制的实现
                if let Err(e) = di_server::run().await {
                    eprintln!("服务启动失败: {e}");
                }
            });
            tauri::async_runtime::spawn(async move {
                if !wait_ready(PORT).await {
                    eprintln!("服务未能就绪");
                    return;
                }
                // 令牌直接放进 URL:桌面版用户不该手动贴令牌
                let token = std::fs::read_to_string(di_server::auth::config_path())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let url = format!("http://127.0.0.1:{PORT}/?t={token}");
                let _ = WebviewWindowBuilder::new(
                    &handle,
                    "main",
                    WebviewUrl::External(url.parse().expect("URL 合法")),
                )
                .title("AI 战略顾问")
                .inner_size(1200.0, 820.0)
                .min_inner_size(900.0, 600.0)
                .build();
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动桌面应用失败");
}
