//! In-process digest cron. Mirrors `net_worth::spawn_snapshot_cron`: spawn one
//! background thread at server startup that ticks every 15 minutes. Uses a
//! dedicated single-threaded Tokio runtime so that the rusqlite `Connection`
//! (which is `!Send`) can be held across the async market-brief fetch without
//! violating the multi-thread runtime's `Send` requirement.

use crate::db::Db;
use crate::digest::{build, deliver, market, schedule};
use chrono::Utc;
use std::path::PathBuf;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(15 * 60);

pub fn spawn_digest_cron(db_path: PathBuf) {
    std::thread::Builder::new()
        .name("digest-cron".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("digest-cron runtime");
            rt.block_on(async move {
                // Small initial delay so startup isn't competing with first requests.
                tokio::time::sleep(Duration::from_secs(30)).await;
                loop {
                    if let Err(e) = run_tick(&db_path).await {
                        tracing::warn!(err = %e, "digest tick failed");
                    }
                    tokio::time::sleep(TICK).await;
                }
            });
        })
        .expect("spawn digest-cron thread");
}

async fn run_tick(db_path: &PathBuf) -> anyhow::Result<()> {
    let db = Db::open(db_path)?;
    let user_ids = db.list_digest_enabled_user_ids()?;
    if user_ids.is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let client = crate::portfolio::quotes::make_client();
    let mut market_brief: Option<market::MarketBriefCacheState> = None;

    for uid in &user_ids {
        let settings = db.get_digest_settings(uid)?;
        if !settings.enabled {
            continue;
        }
        let tz = schedule::parse_tz(&settings.timezone);
        if !schedule::is_due(now, tz, &settings.send_time, settings.last_digest_date.as_deref()) {
            continue;
        }
        let Some(user) = db.get_user_by_id(uid)? else { continue };

        // Generate the shared market brief lazily on the first due user.
        if market_brief.is_none() {
            market_brief = Some(market::MarketBriefCacheState {
                brief: market::ensure_market_brief(&db, &client).await,
            });
        }
        let brief = market_brief.as_ref().and_then(|m| m.brief.clone());

        let digest = match build::build_digest(&db, &user.id, &user.base_currency, now, tz, brief) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(user = %uid, err = %e, "build_digest failed");
                continue;
            }
        };

        let channel = settings.channel.as_str();
        if channel == "in_app" || channel == "both" {
            if let Err(e) = deliver::deliver_in_app(&db, &user.id, &digest) {
                tracing::warn!(user = %uid, err = %e, "deliver_in_app failed");
            }
        }
        if channel == "email" || channel == "both" {
            let _ = deliver::deliver_email(&client, &user.email, &digest).await;
        }

        // Mark sent (user-local date) regardless of email outcome — no retry storm.
        let local_date = now.with_timezone(&tz).format("%Y-%m-%d").to_string();
        if let Err(e) = db.set_last_digest_date(&user.id, &local_date) {
            tracing::warn!(user = %uid, err = %e, "set_last_digest_date failed");
        }
        tracing::info!(user = %uid, channel = %channel, date = %digest.date, "digest sent");
    }
    Ok(())
}
