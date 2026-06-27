//! ai-note · AI-driven personal note app with semantic search.
//!
//! Multi-tenant. Embeddings via Gemini text-embedding-004 + async worker.
//! Same auth shape as ai-ledger (email + invite code), same deploy story
//! (one binary + embedded UI + SQLite next to it).

use async_trait::async_trait;
use clap::Parser;
use harness_core::Model;
use harness_models::{GeminiNative, OpenAiCompat};
use std::path::PathBuf;
use std::sync::Arc;

mod admin;
mod auth;
mod db;
mod embed_slot;
mod embed_worker;
mod pricing;
mod search;
mod server;
mod tools;

#[derive(Parser, Debug)]
#[command(name = "ai-note", version)]
struct Cli {
    /// Serve HTTP on the given socket.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    #[arg(long, default_value_t = 6755)]
    port: u16,
    /// Path to the SQLite database. Overrides $HARNESS_NOTE_DB.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Agent loop budget.
    #[arg(long, default_value_t = 6)]
    max_iters: u32,
    /// Which provider for the chat agent: openai-compat (default — DeepSeek)
    /// or gemini.
    #[arg(long, default_value = "openai-compat")]
    chat_provider: String,
    /// Chat model id.
    #[arg(long, default_value = "deepseek-v4-flash")]
    chat_model: String,
}

/// Erase the concrete chat-model type behind `Arc<dyn Model>` so server can
/// hold it generically. Implementations from harness-models already implement
/// Model; the enum wrapper here keeps stream() routed correctly across the
/// two adapters we expose.
pub(crate) struct AnyModelHandle(pub Arc<dyn Model>);

#[async_trait]
impl Model for AnyModelHandle {
    async fn complete(
        &self,
        ctx: &harness_core::Context,
    ) -> Result<harness_core::ModelOutput, harness_core::ModelError> {
        self.0.complete(ctx).await
    }
    async fn stream(
        &self,
        ctx: &harness_core::Context,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<harness_core::ModelDelta, harness_core::ModelError>,
        >,
        harness_core::ModelError,
    > {
        self.0.stream(ctx).await
    }
    fn info(&self) -> harness_core::ModelInfo {
        self.0.info()
    }
}

fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("HARNESS_NOTE_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".harness-note/notes.db")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(default_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Init DB (creates tables on first launch).
    {
        let _ = db::Db::open(&db_path)?;
    }

    // ── chat model ───────────────────────────────────────────
    let deepseek_key = std::env::var("DEEPSEEK_API_KEY").ok();
    let gemini_key = std::env::var("GEMINI_API_KEY").ok();
    let chat_model: Arc<dyn Model> = match cli.chat_provider.as_str() {
        "gemini" => {
            let key = gemini_key.clone().ok_or_else(|| {
                anyhow::anyhow!("GEMINI_API_KEY required for chat_provider=gemini")
            })?;
            Arc::new(GeminiNative::with_key(
                "https://generativelanguage.googleapis.com",
                &cli.chat_model,
                key,
            ))
        }
        _ => {
            let key = deepseek_key.clone().ok_or_else(|| {
                anyhow::anyhow!("DEEPSEEK_API_KEY required for openai-compat chat")
            })?;
            Arc::new(OpenAiCompat::with_key(
                "https://api.deepseek.com",
                &cli.chat_model,
                key,
            ))
        }
    };
    let model_handle = format!("{}:{}", cli.chat_provider, cli.chat_model);

    // ── embedder ─────────────────────────────────────────────
    // Gemini-only for now. Could swap to OpenAI / Voyage / local later.
    let embed_key = gemini_key
        .clone()
        .ok_or_else(|| anyhow::anyhow!("GEMINI_API_KEY required for embeddings"))?;
    let embedder: Arc<dyn harness_core::Embedder> =
        Arc::new(harness_models::GeminiEmbed::with_key(embed_key));
    embed_slot::set(embedder.clone());

    // ── background embed worker ──────────────────────────────
    embed_worker::EmbedWorker {
        db_path: db_path.clone(),
        embedder: embedder.clone(),
        batch_size: 32,
        idle_pause: std::time::Duration::from_secs(5),
        busy_pause: std::time::Duration::from_millis(250),
    }
    .spawn();

    // ── http server ──────────────────────────────────────────
    let user_tz = std::env::var("HARNESS_USER_TZ")
        .ok()
        .filter(|s| !s.is_empty());

    // Seed provider_config from env on first launch; DB wins on subsequent
    // restarts after admin edits.
    {
        let cfg_db = db::Db::open(&db_path)?;
        if let Some(k) = &deepseek_key {
            cfg_db.provider_config_seed_if_missing("deepseek_api_key", k)?;
        }
        if let Some(k) = &gemini_key {
            cfg_db.provider_config_seed_if_missing("gemini_api_key", k)?;
        }
        cfg_db.provider_config_seed_if_missing("chat_provider", &cli.chat_provider)?;
        cfg_db.provider_config_seed_if_missing("chat_model", &cli.chat_model)?;
        // Seed pricing rate card on first launch. JSON-encoded HashMap; admin
        // UI edits this from the System page.
        let default_pricing = serde_json::to_string(&pricing::default_rate_card())?;
        cfg_db.provider_config_seed_if_missing("pricing_rate_card", &default_pricing)?;
    }
    let stored = {
        let cfg_db = db::Db::open(&db_path)?;
        cfg_db.provider_config_all()?
    };
    let pricing_card: pricing::RateCard = stored
        .get("pricing_rate_card")
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(pricing::default_rate_card);
    let app_cfg = server::AppConfig {
        deepseek_key: stored.get("deepseek_api_key").cloned().or(deepseek_key),
        gemini_key: stored.get("gemini_api_key").cloned().or(gemini_key),
        chat_provider: stored
            .get("chat_provider")
            .cloned()
            .unwrap_or_else(|| cli.chat_provider.clone()),
        chat_model: stored
            .get("chat_model")
            .cloned()
            .unwrap_or_else(|| cli.chat_model.clone()),
        pricing: pricing_card,
    };

    let state = server::AppState {
        db_path,
        model: chat_model,
        embedder,
        max_iters: cli.max_iters,
        model_handle,
        user_tz,
        config: std::sync::Arc::new(std::sync::RwLock::new(app_cfg)),
    };
    let addr: std::net::SocketAddr = format!("{}:{}", cli.bind, cli.port).parse()?;
    println!("→ ai-note");
    println!("  bind:    http://{}", addr);
    println!("  db:      {}", state.db_path.display());
    println!("  model:   {}", state.model_handle);
    println!(
        "  embed:   {} ({}d)",
        state.embedder.handle(),
        state.embedder.dim()
    );

    server::serve(state, addr).await
}
