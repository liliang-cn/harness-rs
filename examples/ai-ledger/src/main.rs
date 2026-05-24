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

mod admin;
mod auth;
mod db;
mod fx;
mod loans;
mod model;
mod net_worth;
mod portfolio;
mod pricing;
mod seo;
mod server;
mod skills;
mod subscription;
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
    // ─── subscriptions (recurring expenses) ───
    "add_subscription",
    "list_subscriptions",
    "cancel_subscription",
    "record_subscription_charge",
    // ─── loans / mortgages / receivables ───
    "add_loan",
    "record_loan_payment",
    "loan_summary",
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
4b. **Capture specifics as `note`.** When the user mentions WHAT they bought / WHO / WHERE \
   beyond the bare category, put it on `note` — e.g. \"火锅\" / \"冰粉\" / \"打车回家\" / \
   \"星巴克\" / \"给妈妈的生日礼物\". Keep notes short (≤ 20 字). If the message has no \
   detail beyond the category (\"今天吃饭 200\"), OMIT `note` — don't pad with the category \
   name, that's redundant.\n\
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
10b. **Long-term memory** — you have access to per-user memory tools:\n\
   • `list_memories(query?, k?)` — see what's already stored about this user. Call \
     this whenever you're about to *guess* a user preference (\"我猜你喜欢…\") instead \
     of just guessing. If the recall returned something, use it; if not, ask.\n\
   • `remember_this(content, tags?, ttl_days?)` — call this when the user says \"记住 X\" \
     / \"以后 \" / \"默认\" / \"my preference is …\". DO NOT call this on routine \
     transactions (those go to `log_transaction`) or specific amounts (the framework \
     blocks them anyway). Good: \"用户偏好按月订阅 SaaS 而不是买断\". Bad: \"用户火锅花了 ¥199\".\n\
   • `forget_memory(id)` — when the user says \"忘掉 X\" / \"that's wrong\" / \"删掉那条\"; \
     first `list_memories` to find the id.\n\
   Per-iteration memory recall is automatic via the framework — the relevant prior \
   context appears at the top of your system prompt. If you don't see what you need, \
   call `list_memories` explicitly with a different query.\n\
11. **Subscriptions (recurring expenses)** — for anything that auto-charges on a fixed \
   cadence (Claude Code $250/月, Netflix, 房租, gym, 域名 yearly, …) use `add_subscription` \
   NOT `log_transaction`. This stores a SCHEDULE (amount + frequency + next charge date) \
   so the daily auto-charger can record each real charge as a transaction automatically.\n\
   Phrases that mean \"add a subscription\": \"我有个 X 订阅，每月/每年 Y 元\", \"包月\", \
   \"按月扣 / 按年付\", \"每月扣 / 每年扣\".\n\
   Phrases that mean \"a known subscription just got charged\": \"X 这个月扣款了\", \
   \"Netflix 刚扣了\", \"yearly renewal hit\" → first `list_subscriptions`, find the id, \
   then `record_subscription_charge(subscription_id=<id>, occurred_at=<date>)`. This \
   creates the transaction AND advances next_charge_date in one shot — do NOT also call \
   `log_transaction`.\n\
   When adding: `currency` is required (USD / CNY / EUR …) — DO NOT guess. `next_charge_date` \
   is required (YYYY-MM-DD). If the user says \"每月 X 元\" without a date, ASK \"下次扣款是哪一天？\".\
   `pay_channel` is optional but valuable for the user — \"Android/Google Play\", \"信用卡 ****1234\", \
   etc. If user mentions one (\"通过 Android 扣的\"), set it.\n\
12. **Loans / 借款 / IOUs.** When the user mentions taking on a new loan \
   (\"我贷了 30 万房贷\", \"I just got a $5k personal loan from BoA\", \"借朋友 1000\"), \
   call `add_loan` with the right `kind`:\n\
     • 房贷 / mortgage / \"a 30-year fixed\"      → kind=mortgage\n\
     • car loan / 车贷 / personal loan / 信用社借款 → kind=loan\n\
     • 借给朋友 / lent to / they owe me            → kind=receivable\n\
   Always confirm with the user: principal + currency + APR + start_date. If they \
   didn't volunteer term_months / monthly_payment, leave them null — don't invent \
   numbers. APR \"0\" is fine for interest-free family IOUs.\n\
   When the user mentions a payment (\"还了 1500 房贷\", \"paid 500 to BoA\"), call \
   `record_loan_payment` with the loan account_id and their cash account_id. If \
   you only know a friendly name, call `loan_summary` first to look up the id.\n\
   For overview questions (\"我现在有哪些贷款\", \"how much do I still owe\", \
   \"我的房贷还剩多少\"), call `loan_summary`. Default is active-only; pass \
   include_paid_off=true if the user asks about retired loans.\n\
   Receivables work symmetrically — \"Alice 还了我 200\" is a `record_loan_payment` \
   on the receivable account; `cash_account_id` is the account that received the money.\n\
13. CRITICAL HONESTY RULE: Never claim a write happened unless you actually called \
   one of the write tools (`add_account`, `log_transaction`, `record_transfer`, \
   `set_budget`, `add_asset`, `record_trade`, `update_price`, `refresh_prices`, \
   `apply_category_merge`, `delete_asset`, `delete_trade`, `delete_transaction`, \
   `add_subscription`, `cancel_subscription`, `record_subscription_charge`, \
   `add_loan`, `record_loan_payment`) \
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

    /// Scan all users' subscriptions; for every active one whose
    /// `next_charge_date <= today` (local), record a transaction and advance
    /// the date. Idempotent across re-runs within the same day (won't
    /// double-charge unless the date is genuinely overdue twice). Suitable
    /// as a daily cron via `harness-rs-daemon` or system cron.
    #[arg(long)]
    auto_charge_subs: bool,

    /// Port for --serve (default: 6743).
    #[arg(long, default_value_t = 6743)]
    port: u16,

    /// Bind address for --serve. Default 127.0.0.1 (local only).
    /// Use 0.0.0.0 to expose externally — no auth is built in, so only do
    /// this behind a reverse proxy or on a private network.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
}

/// Run by `--auto-charge-subs`. Scans every user's active subscriptions; for
/// each one whose `next_charge_date <= today (local)`, inserts a transaction
/// and advances the date by the subscription's frequency. Catches up if a
/// charge was missed for multiple periods (loops until next_charge > today).
///
/// Designed to be invoked by cron / `harness-rs-daemon` daily. Safe to run
/// multiple times per day — once `next_charge_date > today` we stop touching
/// it, so re-runs are no-ops.
async fn run_auto_charge_subs() -> anyhow::Result<()> {
    use chrono::{Local, TimeZone, Utc};
    use rust_decimal::Decimal;
    use uuid::Uuid;

    let db = db::Db::open(&tools::ledger_path())?;
    let today = Local::now().date_naive();
    let due = db.due_subscriptions_all_users(today)?;
    if due.is_empty() {
        println!("→ auto-charge-subs: no subscriptions due as of {today}");
        return Ok(());
    }
    println!("→ auto-charge-subs: {} subscription(s) due as of {today}", due.len());
    let mut charged = 0u32;
    let mut skipped = 0u32;
    for (user_id, sub) in due {
        // Catch up on missed periods, one charge per overdue cycle.
        let mut next = sub.next_charge_date;
        let freq = sub.frequency;
        let mut cycles = 0u32;
        while next <= today && cycles < 12 {
            let occurred_at = Local
                .from_local_datetime(&next.and_hms_opt(15, 0, 0).unwrap())
                .single()
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let note_prefix = format!("[订阅] {} (auto)", sub.name);
            let note = match sub.note.as_deref() {
                Some(n) if !n.is_empty() => format!("{note_prefix} · {n}"),
                _ => note_prefix,
            };
            let txn = crate::model::Transaction {
                id: Uuid::new_v4().to_string()[..8].to_string(),
                kind: crate::model::TxnKind::Expense,
                amount: sub.amount,
                currency: sub.currency.clone(),
                account_id: sub.account_id.clone(),
                counter_account_id: None,
                category: sub.category.clone(),
                note: Some(note),
                occurred_at,
                created_at: Utc::now(),
            };
            if let Err(e) = db.insert_transaction(&user_id, &txn) {
                eprintln!(
                    "  ✗ user={user_id} sub={} (\"{}\"): insert_transaction failed: {e}",
                    sub.id, sub.name
                );
                skipped += 1;
                break;
            }
            charged += 1;
            cycles += 1;
            // Compute the next cycle WITHOUT calling advance_subscription
            // yet (we batch a single update at the end of the catch-up).
            next = freq.advance(next);
            let _ = Decimal::ZERO; // suppress unused warning when Decimal isn't used in this path
        }
        // Persist the final next_charge_date once.
        if cycles > 0 {
            let _ = db
                .conn_update_subscription_next_date(&user_id, &sub.id, next)
                .map_err(|e| {
                    eprintln!("  ✗ user={user_id} sub={}: date-advance failed: {e}", sub.id);
                });
        }
        println!(
            "  ✓ user={user_id} sub={} (\"{}\" {} {}): {} cycle(s) recorded, next={}",
            sub.id, sub.name, sub.amount, sub.currency, cycles, next
        );
    }
    println!("→ auto-charge-subs: {charged} charge(s) recorded, {skipped} skipped");
    Ok(())
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
    build_task_description_with_lang(user_request, history, None)
}

/// Same as `build_task_description` but injects a one-line locale
/// directive at the top so the model speaks the user's UI language by
/// default. The big SYSTEM_PROMPT body covers EN+ZH content already; this
/// header just sets the reply language.
pub(crate) fn build_task_description_with_lang(
    user_request: &str,
    history: &[(String, String)],
    lang: Option<&str>,
) -> String {
    let mut s = String::new();
    if let Some(l) = lang {
        let header = match l {
            "zh" | "zh-CN" | "zh-Hans" | "zh-TW" => {
                "Default reply language: Chinese (Simplified). If the user writes in another language, follow their language instead.\n\n"
            }
            "en" | "en-US" | "en-GB" => {
                "Default reply language: English. If the user writes in another language, follow their language instead.\n\n"
            }
            _ => "",
        };
        s.push_str(header);
    }
    s.push_str(SYSTEM_PROMPT);
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

    if cli.auto_charge_subs {
        return run_auto_charge_subs().await;
    }

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
        // Multi-provider keys. `HARNESS_API_KEY` is the legacy single-key
        // env (used to live in qc-jp's /etc/ai-ledger.env pointing at Gemini)
        // — if set without an explicit DEEPSEEK_API_KEY / GEMINI_API_KEY, we
        // assume it matches the configured `HARNESS_MODEL_PROVIDER`.
        let mut deepseek_key = std::env::var("DEEPSEEK_API_KEY").ok();
        let mut gemini_key = std::env::var("GEMINI_API_KEY").ok();
        let provider_env = std::env::var("HARNESS_MODEL_PROVIDER")
            .unwrap_or_default()
            .to_lowercase();
        if let Ok(legacy) = std::env::var("HARNESS_API_KEY") {
            if provider_env == "gemini" && gemini_key.is_none() {
                gemini_key = Some(legacy);
            } else if deepseek_key.is_none() {
                deepseek_key = Some(legacy);
            }
        }
        let available_models = vec![
            server::ModelOption {
                id: "deepseek-v4-flash".into(),
                label: "DeepSeek v4 Flash".into(),
                provider: "deepseek".into(),
                available: deepseek_key.is_some(),
            },
            server::ModelOption {
                id: "deepseek-v4-pro".into(),
                label: "DeepSeek v4 Pro".into(),
                provider: "deepseek".into(),
                available: deepseek_key.is_some(),
            },
            server::ModelOption {
                id: "gemini-3.5-flash".into(),
                label: "Gemini 3.5 Flash".into(),
                provider: "gemini".into(),
                available: gemini_key.is_some(),
            },
        ];
        // Default model: prefer HARNESS_MODEL if it's both known and
        // available; otherwise pick the first available.
        let want = model_id.to_string();
        let default_model_id = if available_models
            .iter()
            .any(|m| m.id == want && m.available)
        {
            want
        } else {
            available_models
                .iter()
                .find(|m| m.available)
                .map(|m| m.id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no provider keys configured — set DEEPSEEK_API_KEY and/or GEMINI_API_KEY"
                    )
                })?
        };
        // Seed admin-mutable bits from env → DB, then load whatever the DB
        // ends up with (DB wins on subsequent restarts after admin edits).
        let cfg_db = db::Db::open(&tools::ledger_path())?;
        cfg_db.provider_config_seed_if_missing("default_model_id", &default_model_id)?;
        if let Some(k) = &deepseek_key {
            cfg_db.provider_config_seed_if_missing("deepseek_api_key", k)?;
        }
        if let Some(k) = &gemini_key {
            cfg_db.provider_config_seed_if_missing("gemini_api_key", k)?;
        }
        // Seed pricing rate card on first launch; admin edits via /api/admin/config.
        let default_pricing = serde_json::to_string(&pricing::default_rate_card())?;
        cfg_db.provider_config_seed_if_missing("pricing_rate_card", &default_pricing)?;
        let stored = cfg_db.provider_config_all()?;
        drop(cfg_db);

        let pricing_card: pricing::RateCard = stored
            .get("pricing_rate_card")
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(pricing::default_rate_card);
        let mut cfg = server::AppConfig {
            default_model_id: stored
                .get("default_model_id")
                .cloned()
                .unwrap_or(default_model_id),
            available_models,
            deepseek_key: stored.get("deepseek_api_key").cloned().or(deepseek_key),
            gemini_key: stored.get("gemini_api_key").cloned().or(gemini_key),
            pricing: pricing_card,
        };
        cfg.refresh_availability();

        let state = server::AppState {
            profile: profile.clone(),
            max_iters: cli.max_iters,
            task_store,
            config: std::sync::Arc::new(std::sync::RwLock::new(cfg)),
        };

        // Net-worth pipeline: refresh FX rates from exchangerate.host in
        // the background, then run the daily snapshot cron. Both open
        // their own DB connections (rusqlite Connection is !Send), so we
        // just hand them the path.
        let db_path = std::path::PathBuf::from(
            std::env::var("HARNESS_LEDGER_DB").unwrap_or_else(|_| "ledger.db".into()),
        );
        fx::spawn_refresher(db_path.clone());
        net_worth::spawn_snapshot_cron(db_path.clone());
        loans::spawn_accrual_cron(db_path);

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
