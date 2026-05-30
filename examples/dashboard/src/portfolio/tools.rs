use crate::portfolio::model::{
    Asset, AssetClass, Position, PriceQuote, Trade, TradeKind, aggregate_trades,
};
use crate::portfolio::quotes;
use crate::tools::open_db;
use chrono::{DateTime, Local, TimeZone, Utc};
use harness::ToolError;
use harness::prelude::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use uuid::Uuid;

fn mk_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

fn need_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: format!("{field} required"),
        })
}

fn parse_decimal(v: &Value, field: &str) -> Result<Decimal, ToolError> {
    if let Some(s) = v.as_str() {
        return Decimal::from_str(s).map_err(|e| ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: format!("{field}: {e}"),
        });
    }
    if let Some(f) = v.as_f64() {
        return Decimal::try_from(f).map_err(|e| ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: format!("{field}: {e}"),
        });
    }
    if let Some(i) = v.as_i64() {
        return Ok(Decimal::from(i));
    }
    Err(ToolError::InvalidArgs {
        name: "portfolio".into(),
        reason: format!("{field}: not a number"),
    })
}

fn parse_iso_or_today(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .and_then(|n| Local.from_local_datetime(&n).single())
                .map(|d| d.with_timezone(&Utc))
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(15, 0, 0))
                .and_then(|n| Local.from_local_datetime(&n).single())
                .map(|d| d.with_timezone(&Utc))
        })
}

/// Register a tradable asset (US stock, ETF, commodity future, or crypto).
/// `symbol` is the ticker the user mentions (AAPL / BTC / GC=F). For crypto
/// you SHOULD also pass `provider_id` set to the CoinGecko coin id
/// (BTC → "bitcoin", ETH → "ethereum"); without it `refresh_prices` cannot
/// quote that crypto. For stocks/ETFs/commodities, leave provider_id empty.
#[harness::tool(
    name = "add_asset",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "symbol":      {"type": "string", "description": "User-facing ticker, e.g. AAPL, BTC, GC=F."},
            "name":        {"type": "string", "description": "Friendly name, e.g. Apple Inc., Bitcoin, Gold Futures."},
            "asset_class": {"type": "string", "enum": ["stock", "etf", "commodity", "crypto", "other"]},
            "currency":    {"type": "string", "default": "USD", "description": "Quote currency (USD typical for US stocks/crypto)."},
            "provider_id": {"type": "string", "description": "Only for crypto: CoinGecko coin id, e.g. bitcoin, ethereum, solana."}
        },
        "required": ["symbol", "name", "asset_class"]
    }"#
)]
async fn add_asset(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let symbol = need_str(&args, "symbol")?.to_string();
    let name = need_str(&args, "name")?.to_string();
    let class_s = need_str(&args, "asset_class")?;
    let asset_class = AssetClass::parse(class_s).ok_or_else(|| ToolError::InvalidArgs {
        name: "portfolio".into(),
        reason: format!("unknown asset_class `{class_s}`"),
    })?;
    let currency = args
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("USD")
        .to_uppercase();
    let provider_id = args
        .get("provider_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    if let Some(existing) = db
        .get_asset_by_symbol(&uid, &symbol)
        .map_err(|e| ToolError::Exec(e.to_string()))?
    {
        return Ok(ToolResult {
            ok: false,
            content: json!({
                "error": "symbol already exists",
                "existing": existing,
            }),
            trace: None,
        });
    }
    let a = Asset {
        id: mk_id(),
        symbol,
        name,
        asset_class,
        provider_id,
        currency,
        created_at: Utc::now(),
    };
    if crate::tools::is_trial(w) {
        let n = db
            .count_user_assets(&uid)
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        if n >= 3 {
            return Ok(crate::tools::trial_limit_result("assets", n, 3));
        }
    }
    db.insert_asset(&uid, &a)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"added": a}),
        trace: None,
    })
}

/// List all registered tradable assets with their latest known price (or null
/// if never quoted). Use this before any trade or summary to resolve symbols.
#[harness::tool(
    name = "list_assets",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn list_assets(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let assets = db
        .list_assets(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let mut out = Vec::with_capacity(assets.len());
    for a in &assets {
        let latest = db
            .latest_price(&uid, &a.id)
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        out.push(json!({
            "asset": a,
            "latest_price": latest,
        }));
    }
    Ok(ToolResult {
        ok: true,
        content: json!({"count": assets.len(), "assets": out}),
        trace: None,
    })
}

/// Record a buy, sell, or pre-existing baseline ("opening") position. `qty` is
/// always positive; `kind` carries the meaning:
///   • `buy`     — an actual purchase the user made.
///   • `sell`    — an actual sale.
///   • `opening` — pre-existing holding entered as baseline ("我之前就有 100 股 X").
///                 Counts toward qty + cost basis like a buy, but is NOT a
///                 recent trade and won't show in the trade list.
/// `price_per_unit` is the executed price (or the average cost for opening).
/// `fees` defaults to 0. The asset must exist — call `add_asset` first.
#[harness::tool(
    name = "record_trade",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "asset_symbol":   {"type": "string", "description": "Ticker as known to `add_asset`, e.g. AAPL."},
            "kind":           {"type": "string", "enum": ["buy", "sell", "opening"], "description": "buy/sell for actual trades, opening for pre-existing baseline holdings."},
            "qty":            {"description": "Positive decimal string preferred."},
            "price_per_unit": {"description": "Positive decimal string."},
            "currency":       {"type": "string", "description": "Defaults to the asset's quote currency."},
            "fees":           {"description": "Commission / spread fees, default 0. Ignored for opening."},
            "occurred_at":    {"type": "string", "description": "RFC3339 or YYYY-MM-DD. Defaults to now. For opening, prefer the original purchase date if known, else acceptance date."},
            "note":           {"type": "string"}
        },
        "required": ["asset_symbol", "kind", "qty", "price_per_unit"]
    }"#
)]
async fn record_trade(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let symbol = need_str(&args, "asset_symbol")?.to_string();
    let kind_s = need_str(&args, "kind")?;
    let kind = TradeKind::parse(kind_s).ok_or_else(|| ToolError::InvalidArgs {
        name: "portfolio".into(),
        reason: format!("unknown kind `{kind_s}`"),
    })?;
    let qty = parse_decimal(args.get("qty").unwrap(), "qty")?;
    if qty <= Decimal::ZERO {
        return Err(ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: "qty must be positive".into(),
        });
    }
    let price = parse_decimal(args.get("price_per_unit").unwrap(), "price_per_unit")?;
    if price <= Decimal::ZERO {
        return Err(ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: "price_per_unit must be positive".into(),
        });
    }
    let fees = match args.get("fees") {
        Some(v) => parse_decimal(v, "fees")?,
        None => Decimal::ZERO,
    };
    let note = args.get("note").and_then(|v| v.as_str()).map(String::from);
    let occurred_at = match args.get("occurred_at").and_then(|v| v.as_str()) {
        Some(s) => parse_iso_or_today(s).ok_or_else(|| ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: format!("could not parse `{s}`"),
        })?,
        None => Utc::now(),
    };

    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let asset = db
        .get_asset_by_symbol(&uid, &symbol)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: format!("no asset registered for symbol `{symbol}` — call add_asset first"),
        })?;
    let currency = args
        .get("currency")
        .and_then(|v| v.as_str())
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| asset.currency.clone());

    let t = Trade {
        id: mk_id(),
        asset_id: asset.id.clone(),
        kind,
        qty,
        price_per_unit: price,
        currency,
        fees,
        occurred_at,
        note,
        created_at: Utc::now(),
    };
    if crate::tools::is_trial(w) {
        let n = db
            .count_user_trades(&uid)
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        if n >= 20 {
            return Ok(crate::tools::trial_limit_result("trades", n, 20));
        }
    }
    db.insert_trade(&uid, &t)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "trade": t,
            "asset": asset,
        }),
        trace: None,
    })
}

/// List recent trades, optionally filtered to one asset by symbol.
#[harness::tool(
    name = "list_trades",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "asset_symbol": {"type": "string"},
            "limit":        {"type": "integer", "default": 50, "minimum": 1, "maximum": 500}
        }
    }"#
)]
async fn list_trades(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let asset_id = match args.get("asset_symbol").and_then(|v| v.as_str()) {
        Some(sym) => {
            let a = db
                .get_asset_by_symbol(&uid, sym)
                .map_err(|e| ToolError::Exec(e.to_string()))?;
            match a {
                Some(asset) => Some(asset.id),
                None => {
                    return Ok(ToolResult {
                        ok: false,
                        content: json!({"error": format!("no asset for symbol `{sym}`")}),
                        trace: None,
                    });
                }
            }
        }
        None => None,
    };
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .min(500) as usize;
    let trades = db
        .list_trades(&uid, asset_id.as_deref(), limit)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"count": trades.len(), "trades": trades}),
        trace: None,
    })
}

fn compute_positions(
    assets: &[Asset],
    all_trades: &[Trade],
    latest_price_for: impl Fn(&str) -> Result<Option<PriceQuote>, ToolError>,
) -> Result<Vec<Position>, ToolError> {
    let mut out = Vec::with_capacity(assets.len());
    for a in assets {
        let (qty, avg_cost, realized) = aggregate_trades(&a.id, all_trades);
        let price = latest_price_for(&a.id)?;
        let last_price = price.as_ref().map(|p| p.price);
        let last_price_at = price.as_ref().map(|p| p.fetched_at);
        let last_price_source = price.as_ref().map(|p| p.source.clone());
        let market_value = last_price.map(|p| p * qty);
        let unrealized_pl = last_price.map(|p| (p - avg_cost) * qty);
        out.push(Position {
            asset_id: a.id.clone(),
            symbol: a.symbol.clone(),
            name: a.name.clone(),
            asset_class: a.asset_class,
            currency: a.currency.clone(),
            qty,
            avg_cost,
            realized_pl: realized,
            last_price,
            last_price_at,
            last_price_source,
            market_value,
            unrealized_pl,
        });
    }
    Ok(out)
}

/// Compute current positions per asset: qty held, avg cost basis,
/// realized P/L (from past sells), and — if a recent price exists — current
/// market value and unrealized P/L. Positions with qty == 0 are still
/// included so the user can see prior closed-out plays.
#[harness::tool(
    name = "list_positions",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn list_positions(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let assets = db
        .list_assets(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let trades = db
        .all_trades(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let positions = compute_positions(&assets, &trades, |aid| {
        db.latest_price(&uid, aid)
            .map_err(|e| ToolError::Exec(e.to_string()))
    })?;
    Ok(ToolResult {
        ok: true,
        content: json!({"count": positions.len(), "positions": positions}),
        trace: None,
    })
}

/// Sum positions into a single portfolio snapshot:
/// total market value per currency, total unrealized + realized P/L per
/// currency, breakdown by asset class.
#[harness::tool(
    name = "portfolio_summary",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn portfolio_summary(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let assets = db
        .list_assets(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let trades = db
        .all_trades(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let positions = compute_positions(&assets, &trades, |aid| {
        db.latest_price(&uid, aid)
            .map_err(|e| ToolError::Exec(e.to_string()))
    })?;

    use std::collections::HashMap;
    let mut value_by_currency: HashMap<String, Decimal> = HashMap::new();
    let mut realized_by_currency: HashMap<String, Decimal> = HashMap::new();
    let mut unrealized_by_currency: HashMap<String, Decimal> = HashMap::new();
    let mut value_by_class: HashMap<String, Decimal> = HashMap::new();
    let mut missing_prices = Vec::new();
    for p in &positions {
        if p.qty == Decimal::ZERO && p.realized_pl == Decimal::ZERO {
            continue;
        }
        *realized_by_currency
            .entry(p.currency.clone())
            .or_insert(Decimal::ZERO) += p.realized_pl;
        if let Some(mv) = p.market_value {
            *value_by_currency
                .entry(p.currency.clone())
                .or_insert(Decimal::ZERO) += mv;
            *value_by_class
                .entry(format!("{}/{}", p.asset_class.as_str(), p.currency))
                .or_insert(Decimal::ZERO) += mv;
        } else if p.qty > Decimal::ZERO {
            missing_prices.push(p.symbol.clone());
        }
        if let Some(upl) = p.unrealized_pl {
            *unrealized_by_currency
                .entry(p.currency.clone())
                .or_insert(Decimal::ZERO) += upl;
        }
    }
    let stringify = |m: HashMap<String, Decimal>| -> serde_json::Map<String, Value> {
        m.into_iter()
            .map(|(k, v)| (k, json!(v.to_string())))
            .collect()
    };
    Ok(ToolResult {
        ok: true,
        content: json!({
            "market_value_by_currency": stringify(value_by_currency),
            "realized_pl_by_currency":  stringify(realized_by_currency),
            "unrealized_pl_by_currency": stringify(unrealized_by_currency),
            "market_value_by_class_currency": stringify(value_by_class),
            "missing_prices_for": missing_prices,
            "position_count": positions.iter().filter(|p| p.qty > Decimal::ZERO).count(),
        }),
        trace: None,
    })
}

/// Manually set the latest price for an asset (e.g. when the live provider is
/// unreachable or the user wants to fix a quote). Source is recorded as
/// "manual". Does NOT replace future refreshes — `refresh_prices` will
/// overwrite this with a live quote if it succeeds.
#[harness::tool(
    name = "update_price",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "asset_symbol": {"type": "string"},
            "price":        {"description": "Positive decimal price per unit."},
            "currency":     {"type": "string", "description": "Defaults to the asset's quote currency."}
        },
        "required": ["asset_symbol", "price"]
    }"#
)]
async fn update_price(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let symbol = need_str(&args, "asset_symbol")?.to_string();
    let price = parse_decimal(args.get("price").unwrap(), "price")?;
    if price <= Decimal::ZERO {
        return Err(ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: "price must be positive".into(),
        });
    }
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let asset = db
        .get_asset_by_symbol(&uid, &symbol)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "portfolio".into(),
            reason: format!("no asset for `{symbol}`"),
        })?;
    let currency = args
        .get("currency")
        .and_then(|v| v.as_str())
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| asset.currency.clone());
    let q = PriceQuote {
        asset_id: asset.id.clone(),
        price,
        currency,
        fetched_at: Utc::now(),
        source: "manual".into(),
    };
    db.insert_price(&uid, &q)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"set": q, "asset": asset}),
        trace: None,
    })
}

/// Delete an asset and ALL its trades + cached prices. Use when the user
/// says "我没有 X" / "删除 X 的持仓" / "清掉 X" — i.e. the asset was logged by
/// mistake (LLM mis-mapping, dup, scanner spam). This is destructive and
/// non-reversible. Look up the symbol's `asset_id` via `list_assets` first if
/// the user gave only a name.
#[harness::tool(
    name = "delete_asset",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "asset_symbol": {"type": "string", "description": "Ticker, case-insensitive (AAPL / BTC / GC=F)."}
        },
        "required": ["asset_symbol"]
    }"#
)]
async fn delete_asset(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let symbol = need_str(&args, "asset_symbol")?.to_string();
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let asset = db
        .get_asset_by_symbol(&uid, &symbol)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let Some(asset) = asset else {
        return Ok(ToolResult {
            ok: false,
            content: json!({"error": format!("no asset registered for `{symbol}`")}),
            trace: None,
        });
    };
    let (trades_n, prices_n) = db
        .delete_asset(&uid, &asset.id)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "deleted_symbol": asset.symbol,
            "deleted_name": asset.name,
            "trades_deleted": trades_n,
            "prices_deleted": prices_n,
        }),
        trace: None,
    })
}

/// Delete a single trade by id. Use when the user wants to remove ONE wrong
/// entry but keep the asset and other history. Get the id from `list_trades`.
#[harness::tool(
    name = "delete_trade",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "trade_id": {"type": "string", "description": "Trade id from list_trades / record_trade response."}
        },
        "required": ["trade_id"]
    }"#
)]
async fn delete_trade(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let id = need_str(&args, "trade_id")?.to_string();
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let n = db
        .delete_trade(&uid, &id)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: n > 0,
        content: if n > 0 {
            json!({"deleted_trade_id": id})
        } else {
            json!({"error": format!("no trade with id `{id}`")})
        },
        trace: None,
    })
}

/// Look up the current CNY/g price of physical gold (上海黄金交易所 Au9999) via
/// Google Search Grounding, with a 15-minute global cache. Use this for any
/// "金价多少 / 黄金现在多少钱一克 / SGE 现货" question, OR before computing the
/// market value of a CNY-denominated gold position. Cheap on cache hit
/// (single SQLite read); only hits Gemini once per ~15 min globally.
#[harness::tool(
    name = "cny_gold_price",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn cny_gold_price(_args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let path = crate::tools::ledger_path();
    let client = quotes::make_client();
    // The asset shell here only carries currency — the cache key is global.
    let stub = Asset {
        id: "cache-cny-gold".into(),
        symbol: "Au9999".into(),
        name: "上海黄金交易所 Au9999".into(),
        asset_class: AssetClass::Commodity,
        provider_id: None,
        currency: "CNY".into(),
        created_at: Utc::now(),
    };
    let q = quotes::fetch_cny_gold_cached(&client, &stub, &path, quotes::cny_gold_cache_ttl())
        .await
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "symbol": "Au9999",
            "name": "上海黄金交易所 Au9999",
            "price_cny_per_gram": q.price.to_string(),
            "currency": q.currency,
            "fetched_at": q.fetched_at.to_rfc3339(),
            "source": q.source,
        }),
        trace: None,
    })
}

/// Fetch the latest live price for every registered asset (Yahoo Finance for
/// stock/etf/commodity, CoinGecko for crypto) and write it to the local price
/// cache. Returns per-asset success/failure so the model can report partial
/// outages. Network errors are reported, never crashed on.
#[harness::tool(
    name = "refresh_prices",
    risk = "destructive",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn refresh_prices(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let assets = db
        .list_assets(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let client = quotes::make_client();
    let mut report = Vec::with_capacity(assets.len());
    let mut ok_count = 0u32;
    for a in &assets {
        match quotes::fetch_price(&client, a).await {
            Ok(q) => {
                db.insert_price(&uid, &q)
                    .map_err(|e| ToolError::Exec(e.to_string()))?;
                ok_count += 1;
                report.push(json!({
                    "symbol": a.symbol,
                    "ok": true,
                    "price": q.price.to_string(),
                    "currency": q.currency,
                    "source": q.source,
                }));
            }
            Err(e) => {
                report.push(json!({
                    "symbol": a.symbol,
                    "ok": false,
                    "error": e.to_string(),
                }));
            }
        }
    }
    Ok(ToolResult {
        ok: ok_count > 0 || assets.is_empty(),
        content: json!({
            "refreshed": ok_count,
            "total": assets.len(),
            "results": report,
        }),
        trace: None,
    })
}
