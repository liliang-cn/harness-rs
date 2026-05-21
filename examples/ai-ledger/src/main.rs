//! AI ledger — natural-language personal accounting backend, powered by harness-rs.
//!
//! Tools persist to a SQLite file (default `~/.harness-ledger/ledger.db`,
//! override with `HARNESS_LEDGER_DB`). The agent loop drives them via DeepSeek
//! by default, or any OpenAI-compatible endpoint via the standard
//! `HARNESS_BASE_URL` / `HARNESS_MODEL` / `HARNESS_API_KEY` triple — point those
//! at Gemini's OpenAI-compat endpoint to use Gemini Flash 3.5.
//!
//! ```sh
//! DEEPSEEK_API_KEY=sk-... ledger "昨天火锅花了 200，从微信扣的"
//! DEEPSEEK_API_KEY=sk-... ledger "本月吃饭花了多少？"
//! DEEPSEEK_API_KEY=sk-... ledger --repl
//! ```

mod auth;
mod db;
mod model;
mod portfolio;
mod server;
mod skills;
mod tools;

use async_trait::async_trait;
use clap::Parser;
use harness::prelude::*;
use harness_context::with_profile;
use harness_core::{
    Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, Model, UserProfile,
};
use harness_loop::{AgentLoop, Outcome, ProfileGuide};
use harness_models::{GeminiNative, OpenAiCompat, providers::DEEPSEEK};
use harness_skills::SkillRegistry;
// Force-link harness-rs-tools-web so its `#[tool]` registrations land in
// `inventory` — picks up `web_search` and `web_fetch`.
use harness_tools_web as _;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub(crate) const TOOL_NAMES: &[&str] = &[
    // ─── ledger ───
    "current_time",
    "list_accounts",
    "add_account",
    "log_transaction",
    "record_transfer",
    "list_transactions",
    "monthly_report",
    "set_budget",
    "check_budgets",
    "list_categories",
    "suggest_category_merges",
    "apply_category_merge",
    "delete_transaction",
    // ─── portfolio (investments — separate from ledger) ───
    "add_asset",
    "list_assets",
    "record_trade",
    "list_trades",
    "list_positions",
    "portfolio_summary",
    "update_price",
    "refresh_prices",
    "cny_gold_price",
    "delete_asset",
    "delete_trade",
    // ─── web access (from harness-rs-tools-web) ───
    "web_search",
    "web_fetch",
];

/// Guide that surfaces the catalogue of `#[skill]`-registered playbooks to the
/// LLM. The catalogue text is built once at startup.
pub(crate) struct SkillsCatalogueGuide(String);

static SKILLS_GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static SKILLS_GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

impl SkillsCatalogueGuide {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let registry = SkillRegistry::new().with_macro_skills()?;
        let text = if registry.is_empty() {
            String::new()
        } else {
            format!(
                "Available skills (multi-step playbooks). Use a skill's procedure when its description matches the user's request:\n{}",
                registry.catalogue()
            )
        };
        Ok(Self(text))
    }
}

#[async_trait]
impl Guide for SkillsCatalogueGuide {
    fn id(&self) -> &GuideId {
        SKILLS_GUIDE_ID.get_or_init(|| "ai-ledger-skills-catalogue".into())
    }
    fn kind(&self) -> Execution {
        Execution::Inferential
    }
    fn scope(&self) -> &GuideScope {
        SKILLS_GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        if !self.0.is_empty() {
            ctx.guides.push(Block::Text(self.0.clone()));
        }
        Ok(())
    }
}

/// Dispatch wrapper that lets AgentLoop pick between OpenAI-compat and
/// Gemini-native at runtime. Selected by `HARNESS_MODEL_PROVIDER`:
///   `gemini` → native Google API + Search Grounding tool
///   anything else (or unset) → OpenAI-compat (DeepSeek, OpenAI, Gemini-via-shim, …)
pub(crate) enum AnyModel {
    OpenAi(OpenAiCompat),
    Gemini(GeminiNative),
}

#[async_trait]
impl harness_core::Model for AnyModel {
    async fn complete(
        &self,
        ctx: &harness_core::Context,
    ) -> Result<harness_core::ModelOutput, harness_core::ModelError> {
        match self {
            AnyModel::OpenAi(m) => m.complete(ctx).await,
            AnyModel::Gemini(m) => m.complete(ctx).await,
        }
    }
    // Without this, `AgentLoop::with_streaming(true)` runs through the default
    // trait stream() impl — which just calls complete() then emits one big
    // Text delta. The real per-token streaming in OpenAiCompat is bypassed.
    async fn stream(
        &self,
        ctx: &harness_core::Context,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<harness_core::ModelDelta, harness_core::ModelError>>,
        harness_core::ModelError,
    > {
        match self {
            AnyModel::OpenAi(m) => m.stream(ctx).await,
            AnyModel::Gemini(m) => m.stream(ctx).await,
        }
    }
    fn info(&self) -> harness_core::ModelInfo {
        match self {
            AnyModel::OpenAi(m) => m.info(),
            AnyModel::Gemini(m) => m.info(),
        }
    }
}

pub(crate) fn build_model(base_url: &str, model_id: &str, api_key: String) -> AnyModel {
    let provider = std::env::var("HARNESS_MODEL_PROVIDER")
        .unwrap_or_default()
        .to_lowercase();
    if provider == "gemini" {
        AnyModel::Gemini(GeminiNative::with_key(model_id, api_key).with_search_grounding(true))
    } else {
        AnyModel::OpenAi(OpenAiCompat::with_key(
            base_url.to_string(),
            model_id,
            api_key,
        ))
    }
}

pub(crate) fn collect_tools() -> Vec<Arc<dyn Tool>> {
    use harness_core::iter_macro_tools;
    // When the agent runs on Gemini-native, hide the DDG/Bing-scraping web
    // tools — Gemini's built-in Google Search grounding does the same job
    // without the IP-blocking pain.
    let is_gemini = std::env::var("HARNESS_MODEL_PROVIDER")
        .map(|v| v.eq_ignore_ascii_case("gemini"))
        .unwrap_or(false);
    iter_macro_tools()
        .filter(|t| {
            if !TOOL_NAMES.contains(&t.name()) {
                return false;
            }
            if is_gemini && (t.name() == "web_search" || t.name() == "web_fetch") {
                return false;
            }
            true
        })
        .collect()
}

pub(crate) const SYSTEM_PROMPT: &str = "\
You are a personal bookkeeping assistant. The user types short natural-language \
descriptions of spending, income, or transfers — sometimes in Chinese, sometimes \
in English. Your job: extract structured facts and call the right tool.\n\
\n\
Hard rules:\n\
1. For relative time (今天/昨天/上周/this month/last Friday), call `current_time` \
   FIRST to anchor the date.\n\
2. Before logging an expense or income, you usually need an `account_id`. If unsure, \
   call `list_accounts`. If the user mentions a new payment channel that doesn't \
   exist (e.g. \"信用卡\"), ask them whether to create it — don't silently invent.\n\
3. Categories are free-form Chinese or English. Reuse existing names where they fit \
   (call `list_categories` if you're not sure what's been used). Only create a new \
   category when nothing existing fits.\n\
4. Amounts are positive numbers regardless of expense vs income — the `kind` field \
   carries the sign.\n\
5. Transfers between the user's own accounts → `record_transfer`, NOT \
   `log_transaction`. Don't double-count.\n\
6. For queries (\"本月吃饭花了多少\", \"剩多少预算\"), use `monthly_report` / \
   `check_budgets` / `list_transactions`, then summarise in one short paragraph \
   with the actual numbers.\n\
7. When the user asks to clean up / merge / consolidate categories, FIRST call \
   `suggest_category_merges` to surface candidate pairs and low-usage outliers, \
   then propose specific (from → to) merges to the user in plain language. Only \
   call `apply_category_merge` AFTER they confirm the direction. Default to \
   merging the lower-usage name into the higher-usage canonical name.\n\
8. **Portfolio (investments) is a SEPARATE domain from the ledger.** The user has a \
   second set of tools for tracking stocks / ETFs / commodities / crypto: \
   `add_asset`, `list_assets`, `record_trade`, `list_trades`, `list_positions`, \
   `portfolio_summary`, `update_price`, `refresh_prices`. Never mix the two — \
   buying AAPL is NOT a ledger expense; it's a `record_trade`. Subrules:\n\
   a. When the user says something like \"我昨天在 398.23 卖了 100 股 Apple\", call \
      `list_assets` first. If the asset isn't registered, proceed to `add_asset` + \
      `record_trade` IF the user's name maps unambiguously to a well-known ticker \
      (Apple→AAPL, 苹果→AAPL, Microsoft/微软→MSFT, Tesla/特斯拉→TSLA, Google/谷歌→GOOGL, \
      Amazon/亚马逊→AMZN, Nvidia/英伟达→NVDA, Bitcoin/比特币/BTC→symbol BTC + \
      provider_id=bitcoin, Ethereum/以太/ETH→ETH + provider_id=ethereum, Gold/黄金→GC=F). \
      State the chosen symbol explicitly in your reply (\"已映射为 AAPL\"). For \
      ambiguous names (\"GS\", \"工行\", custom ETFs), ASK before adding.\n\
   b. For crypto, `add_asset` MUST include `provider_id` set to the CoinGecko coin id \
      (BTC → bitcoin, ETH → ethereum, SOL → solana). Without it, `refresh_prices` \
      can't quote that asset.\n\
   b2. Commodities use Yahoo-style futures symbols: 黄金/Gold → `GC=F` (asset_class=\
      commodity, currency=USD), 白银/Silver → `SI=F`, 原油/Crude → `CL=F`, 铜/Copper → \
      `HG=F`. The framework strips `=F` and falls back to Tencent's hf_* endpoint when \
      Yahoo blocks. For gold ETF specifically (黄金ETF), prefer `GLD` as asset_class=etf \
      instead — it's a stock ticker and uses the stock quote path.\n\
   c. `qty` and `price_per_unit` are always positive; the `kind` carries the meaning:\n\
      • **buy / sell** — actual trades the user just did or recently did with a \
        specific date. Phrases: \"今天买的\", \"刚卖了\", \"昨天 198 买入\", \"上周清仓\".\n\
      • **opening** — pre-existing baseline holdings the user already had before \
        starting to use this tool. Phrases: \"我有 100 股 X\", \"我之前就持有\", \
        \"现在持仓 50 股\", \"账户里有 X\", \"long-term hold\". Opening counts toward \
        qty + cost basis but does NOT show up under \"recent trades\".\n\
      • **Ambiguous** (\"上次买的\", \"前阵子\"): ASK \"这是这次新交易，还是录入历史持仓？\" \
        before calling.\n\
      • Always include `occurred_at` for buy/sell. If the user didn't say, ASK \
        \"是什么时候交易的？\" — do not silently default to today for actual trades.\n\
   d. When the user asks \"AAPL 现在多少 / 我浮盈多少 / 大盘怎么样\", call `refresh_prices` \
      first to get a fresh quote, then `list_positions` or `portfolio_summary` for \
      the numbers. Refresh is a write (it caches), but it's fine to call freely.\n\
      For a pure \"今天黄金多少钱一克 / SGE 金价\" question that doesn't need the \
      portfolio numbers, call `cny_gold_price` instead — it's the single-asset \
      Gemini-grounded path and is globally cached (15 min) so it's cheap.\n\
   e. **Undo / delete is supported.** When the user says \"删除 AAPL 的持仓 / 我没有 X / \
      清掉 X / 这条记错了\":\n\
      • Whole asset (and its full trade history): `delete_asset` (asset gets removed, \
        all trades + prices cascade-deleted).\n\
      • Single bad trade: `list_trades` to find the id, then `delete_trade`.\n\
      • Single bad ledger expense/income: `list_transactions` to find the id, then \
        `delete_transaction`.\n\
      Do not refuse delete requests — the tools exist. Confirm scope (\"是删整支股票还是\
      某一笔交易？\") only when ambiguous.\n\
9. **Web access is available, but the IP may be blocked by search engines.** Tools:\n\
   • `web_search` — DDG → Bing fallback. May return `engines_tried` with \
     `\"anti-bot anomaly\"` / `\"captcha\"` errors when both engines block this IP.\n\
   • `web_fetch` — GET a URL, strip HTML → readable text. May 429/403 on Yahoo \
     Finance, investing.com, etc. (geo-blocked).\n\
   **Stop-after-2 rule:** if `web_search` returns 0 results OR errors that mention \
   `anomaly` / `captcha` / `rate-limit` on the FIRST call, do at most ONE retry \
   with a different query. If the second also fails, STOP — tell the user honestly \
   \"网络搜索从这台服务器出去被反爬墙挡了\"; don't loop. Same for `web_fetch`: if a \
   URL returns 4xx/5xx twice, do not keep trying variants of it.\n\
   Prefer the ledger / portfolio tools FIRST for personal data. Only reach for \
   web_* when the question genuinely needs external info (e.g. \"美联储下次会议\").\n\
10. **Background / scheduled tasks** — when the user wants something deferred or recurring \
   (\"每月 1 号生成账单\" / \"每周给我发个简报\" / \"明早提醒我对账\"), use the tasks_* \
   family: `tasks_create` queues it (kind=one_off or recurring, with a `schedule` like \
   `daily 08:00` / `weekly mon 09:00` / `every 1h`), `tasks_list` shows what's pending, \
   `tasks_get` fetches one, `tasks_cancel` stops it. argv is the command for the runner \
   to execute — typically [\"ledger\", \"--brief\"] or similar. Don't claim a task is \
   running unless you successfully called `tasks_create`.\n\
11. CRITICAL HONESTY RULE: Never claim a write happened unless you actually called \
   one of the write tools (`add_account`, `log_transaction`, `record_transfer`, \
   `set_budget`, `add_asset`, `record_trade`, `update_price`, `refresh_prices`, \
   `apply_category_merge`, `delete_asset`, `delete_trade`, `delete_transaction`) \
   in the CURRENT session. Read-only tools (`current_time`, \
   `list_*`, `monthly_report`, `check_budgets`) do NOT count as writes. If the \
   user described a spend / income / transfer / budget change, you MUST emit at \
   least one write-tool call BEFORE replying with phrases like \"已记录\" / \
   \"logged\" / \"saved\" / \"done\". Reconnaissance is never enough — you must \
   call the write tool.\n\
\n\
Reply style: one or two short sentences confirming what was recorded, or a tight \
bullet list for queries. No preamble, no apologies. Use the user's currency.";

const BRIEF_PROMPT: &str = "\
Compose my monthly money brief. Steps:\n\
1. Call `current_time` to anchor the year/month.\n\
2. Call `monthly_report` for the current month.\n\
3. Call `check_budgets` for the current month.\n\
4. Reply in 4-8 lines max:\n\
   • 本月总支出 by currency\n\
   • Top 3 categories\n\
   • Over-budget categories with by-how-much\n\
   • One short observation (week-on-week trend, unusual category, etc.) — only \
     if the data supports it. Otherwise skip the observation.";

#[derive(Parser, Debug)]
#[command(
    name = "ledger",
    about = "AI-driven personal ledger powered by harness-rs."
)]
struct Cli {
    /// Natural-language request, joined by spaces.
    #[arg(default_values_t = vec!["本月各类花销总结一下".to_string()])]
    task: Vec<String>,

    #[arg(long, default_value = "pro")]
    tier: String,

    #[arg(long, default_value_t = 10)]
    max_iters: u32,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    tz: Option<String>,

    #[arg(long)]
    locale: Option<String>,

    #[arg(long)]
    repl: bool,

    #[arg(long)]
    brief: bool,

    #[arg(long)]
    progress: bool,

    #[arg(long)]
    record: Option<PathBuf>,

    /// Boot an HTTP dashboard (chat + live transactions + report) on --port.
    /// Disables CLI/REPL prompt parsing.
    #[arg(long)]
    serve: bool,

    /// Port for --serve (default: 6743).
    #[arg(long, default_value_t = 6743)]
    port: u16,

    /// Bind address for --serve. Default 127.0.0.1 (local only).
    /// Use 0.0.0.0 to expose externally — no auth is built in, so only do
    /// this behind a reverse proxy or on a private network.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
}

fn build_profile(cli: &Cli) -> UserProfile {
    UserProfile {
        name: cli
            .name
            .clone()
            .or_else(|| std::env::var("HARNESS_USER_NAME").ok()),
        tz: cli
            .tz
            .clone()
            .or_else(|| std::env::var("HARNESS_USER_TZ").ok()),
        locale: cli
            .locale
            .clone()
            .or_else(|| std::env::var("HARNESS_USER_LOCALE").ok()),
        ..Default::default()
    }
}

pub(crate) fn build_task_description(user_request: &str, history: &[(String, String)]) -> String {
    let mut s = SYSTEM_PROMPT.to_string();
    if !history.is_empty() {
        s.push_str("\n\nPrior conversation (oldest first):\n");
        for (role, text) in history {
            let clipped: String = text.chars().take(400).collect();
            s.push_str(&format!("[{role}] {clipped}\n"));
        }
    }
    s.push_str(&format!("\n[user] {user_request}"));
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Minimal tracing setup — picks up RUST_LOG, logs to stderr (journal-friendly).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,reqwest=warn,hyper=warn")))
        .with_target(false)
        .try_init();
    let cli = Cli::parse();

    let api_key = std::env::var("HARNESS_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set HARNESS_API_KEY or DEEPSEEK_API_KEY"))?;
    let base_url = std::env::var("HARNESS_BASE_URL").unwrap_or_else(|_| DEEPSEEK.to_string());
    let default_model_id = match cli.tier.as_str() {
        "flash" => "deepseek-v4-flash",
        _ => "deepseek-v4-pro",
    };
    let model_id_owned =
        std::env::var("HARNESS_MODEL").unwrap_or_else(|_| default_model_id.to_string());
    let model_id: &str = &model_id_owned;

    let info_model = build_model(&base_url, model_id, api_key.clone());
    let info = info_model.info();
    drop(info_model);

    let tools = collect_tools();
    let profile = build_profile(&cli);

    println!(
        "→ ledger\n  model:  {} ({}/{})\n  tools:  {} registered\n  db:     {}",
        info.handle,
        info.provider,
        info.model,
        tools.len(),
        tools::ledger_path().display(),
    );
    if profile.name.is_some() || profile.tz.is_some() {
        println!("  profile: {}", profile.summary_line());
    }
    if cli.repl {
        println!("  mode:    REPL (Ctrl-D / bye / quit / exit)");
    }
    let progress = cli.progress
        || std::env::var("HARNESS_PROGRESS")
            .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
            .unwrap_or(false);
    if progress {
        println!("  progress: live (stderr)");
    }
    if let Some(p) = &cli.record {
        println!("  record:  {}", p.display());
    }
    println!();

    if cli.serve {
        let ip: std::net::IpAddr = cli
            .bind
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid --bind `{}`: {e}", cli.bind))?;
        let addr = std::net::SocketAddr::new(ip, cli.port);
        let task_store: std::sync::Arc<dyn harness_tools_tasks::TaskStore> = {
            let tasks_path = tools::ledger_path()
                .parent()
                .map(|p| p.join("tasks.json"))
                .unwrap_or_else(|| std::path::PathBuf::from("tasks.json"));
            std::sync::Arc::new(harness_tools_tasks::JsonFileStore::new(tasks_path))
        };
        let state = server::AppState {
            base_url: base_url.clone(),
            model_id: model_id.to_string(),
            api_key: api_key.clone(),
            profile: profile.clone(),
            max_iters: cli.max_iters,
            provider_label: info.provider.clone(),
            model_label: info.model.clone(),
            task_store,
        };
        return server::serve(state, addr).await;
    }

    if cli.repl {
        run_repl(
            &base_url,
            model_id,
            api_key,
            tools,
            profile,
            cli.max_iters,
            progress,
            cli.record,
        )
        .await
    } else {
        let user_request = if cli.brief {
            BRIEF_PROMPT.to_string()
        } else {
            cli.task.join(" ")
        };
        run_once(
            &base_url,
            model_id,
            api_key,
            tools,
            profile,
            cli.max_iters,
            user_request,
            progress,
            cli.record,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    base_url: &str,
    model_id: &str,
    api_key: String,
    tools: Vec<Arc<dyn Tool>>,
    profile: UserProfile,
    max_iters: u32,
    user_request: String,
    progress: bool,
    record: Option<PathBuf>,
) -> anyhow::Result<()> {
    let model = build_model(base_url, model_id, api_key.clone());
    let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
    if let Ok(g) = SkillsCatalogueGuide::new() {
        loop_ = loop_.with_guide(Arc::new(g));
    }
    for t in tools {
        loop_ = loop_.with_tool(t);
    }
    if progress {
        loop_ = loop_.with_hook(Arc::new(harness_loop::LiveProgressHook::new()));
    }
    if let Some(p) = record {
        let rec = harness_loop::SessionRecorder::new(&p)
            .map_err(|e| anyhow::anyhow!("recorder: {e}"))?;
        loop_ = loop_.with_hook(Arc::new(rec));
    }
    let mut world = with_profile(".", profile);
    let task = Task {
        description: build_task_description(&user_request, &[]),
        source: None,
        deadline: None,
    };
    match loop_
        .run_with_max_iters(task, &mut world, max_iters)
        .await?
    {
        Outcome::Done { text, iters, .. } => {
            println!("✓ done after {iters} iteration(s)\n");
            if let Some(t) = text {
                println!("{t}");
            }
        }
        Outcome::BudgetExhausted {
            iters, last_text, ..
        } => {
            eprintln!("✗ budget exhausted after {iters} iteration(s)");
            if let Some(t) = last_text {
                eprintln!("\n— forced-synthesis answer —\n{t}");
            }
            std::process::exit(2);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_repl(
    base_url: &str,
    model_id: &str,
    api_key: String,
    tools: Vec<Arc<dyn Tool>>,
    profile: UserProfile,
    max_iters: u32,
    progress: bool,
    record: Option<PathBuf>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    let mut history: Vec<(String, String)> = Vec::new();
    const EXIT_WORDS: &[&str] = &["bye", "quit", "exit", ":q", "\\q"];
    const MAX_HISTORY_TURNS: usize = 20;

    loop {
        stdout.write_all(b"\nyou> ").await?;
        stdout.flush().await?;

        let Some(line) = stdin.next_line().await? else {
            println!("\nbye.");
            break;
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if EXIT_WORDS.contains(&input.to_lowercase().as_str()) {
            println!("bye.");
            break;
        }

        let history_for_call = if history.len() > MAX_HISTORY_TURNS {
            &history[history.len() - MAX_HISTORY_TURNS..]
        } else {
            &history[..]
        };

        let model = build_model(base_url, model_id, api_key.clone());
        let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
    if let Ok(g) = SkillsCatalogueGuide::new() {
        loop_ = loop_.with_guide(Arc::new(g));
    }
        for t in tools.iter().cloned() {
            loop_ = loop_.with_tool(t);
        }
        if progress {
            loop_ = loop_.with_hook(Arc::new(harness_loop::LiveProgressHook::new()));
        }
        if let Some(p) = &record {
            let rec = harness_loop::SessionRecorder::new(p)
                .map_err(|e| anyhow::anyhow!("recorder: {e}"))?;
            loop_ = loop_.with_hook(Arc::new(rec));
        }
        let mut world = with_profile(".", profile.clone());

        let task = Task {
            description: build_task_description(input, history_for_call),
            source: None,
            deadline: None,
        };

        match loop_.run_with_max_iters(task, &mut world, max_iters).await {
            Ok(Outcome::Done { text, iters, .. }) => {
                let response = text.unwrap_or_else(|| "(no response)".into());
                println!("\nasst ({iters} iter)> {response}");
                history.push(("user".into(), input.to_string()));
                history.push(("asst".into(), response));
            }
            Ok(Outcome::BudgetExhausted {
                iters, last_text, ..
            }) => {
                eprintln!("\nasst> ✗ ran out of budget after {iters} iterations.");
                if let Some(t) = last_text {
                    println!("\nasst (forced-synthesis)> {t}");
                }
            }
            Err(e) => eprintln!("\nasst> ✗ error: {e:#}"),
        }
    }
    Ok(())
}
