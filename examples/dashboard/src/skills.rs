//! Three `#[skill]`-registered playbooks ai-ledger exposes to the LLM.
//!
//! These are output-only: the function body does nothing — the value is in the
//! doc comment, which the framework surfaces as the skill's description. The
//! LLM picks the right skill by reading the descriptions and follows the
//! playbook by chaining the named `#[tool]`s.

use harness::SkillError;
use harness::prelude::*;

/// Compose a monthly money brief. Use when the user asks "本月小结 / 月度总结 /
/// monthly brief / 这个月怎么样 / 复盘下本月". Procedure:
/// 1. Call `current_time` to anchor year + month.
/// 2. Call `monthly_report` for the current month.
/// 3. Call `check_budgets` for the current month.
/// 4. Reply in 4-8 lines: total spend per currency, top 3 categories,
///    over-budget categories with by-how-much, one short observation only if
///    the data clearly supports one. No filler. Use the user's locale.
#[harness::skill(
    name = "monthly-money-brief",
    license = "MIT",
    harness(kind = "computational", risk = "read-only")
)]
async fn monthly_money_brief(_ctx: &mut Context, _world: &mut World) -> Result<(), SkillError> {
    Ok(())
}

/// Check overall portfolio health. Use when the user asks "我的组合怎么样 / 大盘
/// 怎么样 / 我浮盈多少 / how's my portfolio / 投资复盘". Procedure:
/// 1. Call `refresh_prices` — quotes come from Yahoo with Tencent fallback for
///    US stocks; CoinGecko for crypto. Report partial failures honestly.
/// 2. Call `portfolio_summary` for totals per currency + per asset class.
/// 3. Call `list_positions` and surface: total market value, total unrealized
///    P/L, top winner + top loser by absolute P/L (skip ones with no quote),
///    any position with drawdown > 15% from avg cost.
/// 4. Reply: 5-10 lines, structured per currency. Never invent numbers; if
///    `missing_prices_for` is non-empty, mention which symbols still need
///    a manual `update_price` or another refresh.
#[harness::skill(
    name = "portfolio-health-check",
    license = "MIT",
    harness(kind = "computational", risk = "read-only")
)]
async fn portfolio_health_check(_ctx: &mut Context, _world: &mut World) -> Result<(), SkillError> {
    Ok(())
}

/// Import the user's pre-existing portfolio baseline. Use when they want to
/// enter EXISTING holdings, NOT new trades — "我之前就有这些 / 录入历史持仓 /
/// set up my current holdings". Procedure:
/// 1. Confirm scope: "历史持仓（开仓基线）还是最近的买卖？" — proceed only
///    when they confirm baseline.
/// 2. For each holding:
///    a. `list_assets` to check if registered; if not `add_asset` (set
///       provider_id to the CoinGecko coin id for crypto).
///    b. `record_trade` with **kind="opening"** + qty + cost basis. Accept
///       whatever date they give for `occurred_at`, or omit.
/// 3. After all are in: `refresh_prices` + `list_positions`, then summarise
///    market value and unrealized P/L.
/// 4. NEVER use kind=buy for these — opening entries are excluded from the
///    "交易记录" UI by design.
#[harness::skill(
    name = "import-historical-positions",
    license = "MIT",
    harness(kind = "computational", risk = "destructive")
)]
async fn import_historical_positions(
    _ctx: &mut Context,
    _world: &mut World,
) -> Result<(), SkillError> {
    Ok(())
}
