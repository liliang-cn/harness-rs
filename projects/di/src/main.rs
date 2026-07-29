//! di-server 可执行入口:服务逻辑都在库里(`lib.rs`),
//! 这样 Tauri 桌面壳能复用同一份实现,而不是复制一遍。
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    di_server::run().await
}
