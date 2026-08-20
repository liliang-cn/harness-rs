//! Deterministic natural-language date/time resolution for agents.
//!
//! An agent that hears "下周五下班前" and writes it to a database must first
//! fix it to an absolute instant. This crate does that step with rules only —
//! **zero LLM, zero network, zero ambient clock**. When the input doesn't
//! match a supported pattern, [`resolve`] returns `None` instead of guessing;
//! the caller decides whether to fall back to a model.
//!
//! # Two layers
//!
//! - **Library**: [`resolve`]`(text, now)` — pure function. `now` is always
//!   supplied by the caller (testability); the resolution timezone is the
//!   offset carried by `now`.
//! - **Tool**: [`ResolveDatetimeTool`] — the `resolve_datetime` tool for the
//!   agent loop. Only here may `now` default to the system clock.
//!
//! # Supported patterns (Chinese + English)
//!
//! | Category | Examples |
//! |---|---|
//! | Relative days | 今天 / 明天 / 后天 / 大后天 / 3天后 / 两周后 / today / tomorrow / day after tomorrow / in 3 days / in 2 weeks |
//! | Weekdays | 周三 / 这周五 / 下周三 / 下下周五 / friday / this friday / next friday |
//! | Calendar dates | 6月20号 / 2026年12月31日 / 三月八号 |
//! | Clock times | 下午3点半 / 早上六点四十 / 晚上12点 / 凌晨三点 / 8:20 / 23:59 / at 3pm / 3:30pm / noon / midnight |
//! | Relative times | 半小时后 / 一刻钟后 / 45分钟后 / 两个小时后 / in 30 minutes / in 2 hours / in half an hour |
//! | Recurrence | 每天 / 每周一三五 / 每月15号 / 每年3月8号 / 工作日 / every day / every monday / weekdays |
//! | Conventions | 下班前 → 18:00, 睡前 → 22:00 (see [`Conventions`]) |
//!
//! # Resolution rules (all deterministic, all documented)
//!
//! - **Time-only** ("下午3点"): the next occurrence — today if still ahead of
//!   `now`, otherwise tomorrow.
//! - **Date-only** ("明天", "3月8号"): midnight of that day, with
//!   [`Resolution::date_only`] set so callers know no time-of-day was given.
//! - **Bare 1–12点 without 上午/下午** ("三点"): ambiguous → `None`. A bare
//!   `H:MM` ("8:20") is taken literally as a 24-hour clock time.
//! - **周X / friday** (no qualifier): the nearest future occurrence.
//!   **这周X / this friday**: within the current Monday-started week, even if
//!   already past. **下周X / next friday**: the next calendar week.
//! - **X月X日 without a year**: the next occurrence (this year if not yet
//!   past, else next year; 2月29 skips to the next leap year).
//! - **晚上12点**: midnight at the end of today, i.e. tomorrow 00:00.
//! - **Recurrences** resolve `start` to the first occurrence and carry the
//!   rule in [`Resolution::recurrence`].
//!
//! # Explicitly out of scope (returns `None`, never a guess)
//!
//! - Interval recurrence: 每两小时 / 每隔20分钟 / every 2 hours / hourly
//! - Multiple times per day: 早中晚三次 / 一天三次; time ranges: 一点到六点
//! - Fuzzy words with no fixed convention: 周末, 尽快, 傍晚 alone, sometime
//! - Holiday-aware rules: 节假日不要响, 调休 (needs a holiday calendar)
//! - Bare ambiguous hours: 三点 (am or pm?)

mod parser;
mod tool;

use chrono::{DateTime, FixedOffset, NaiveTime};
use serde::{Deserialize, Serialize};

pub use tool::ResolveDatetimeTool;

/// The outcome of a successful parse.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolution {
    /// The resolved absolute instant, in the offset of the `now` passed in.
    /// For date-only inputs this is midnight of that day; for recurrences it
    /// is the first occurrence.
    pub start: DateTime<FixedOffset>,
    /// True when the input carried no time-of-day ("明天", "每月15号").
    pub date_only: bool,
    /// Present when the input described a repeating schedule.
    pub recurrence: Option<Recurrence>,
    /// The fragments of the input that were actually consumed, in text order
    /// ("明天", "下午3点"). Lets the caller show *what* was understood.
    pub matched: Vec<String>,
}

/// A repeating schedule. Weekdays are ISO: 1 = Monday … 7 = Sunday.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "freq", rename_all = "snake_case")]
pub enum Recurrence {
    /// 每天 / every day.
    Daily,
    /// 每周X / 工作日 / every monday / weekdays.
    Weekly { days: Vec<u8> },
    /// 每月X号.
    Monthly { day: u32 },
    /// 每年X月X日.
    Yearly { month: u32, day: u32 },
}

/// Optional phrase → clock-time table for household conventions.
///
/// Defaults (documented contract, override with [`resolve_with`]):
///
/// | phrase | time |
/// |---|---|
/// | 下班前 | 18:00 |
/// | 睡前   | 22:00 |
#[derive(Debug, Clone)]
pub struct Conventions {
    /// Literal phrases and the clock time each stands for.
    pub phrases: Vec<(String, NaiveTime)>,
}

impl Default for Conventions {
    fn default() -> Self {
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).expect("static time");
        Self {
            phrases: vec![("下班前".into(), t(18, 0)), ("睡前".into(), t(22, 0))],
        }
    }
}

/// Resolve a natural-language date/time expression against `now`.
///
/// Deterministic: same `(text, now)` in, same answer out — no clock, no
/// randomness, no model. Returns `None` when nothing parseable was found *or*
/// when the text shows evidence of a time expression the rules don't cover
/// (better to hand the whole utterance to a fallback than to half-parse it).
///
/// The timezone of the result is the offset carried by `now`.
///
/// ```
/// use chrono::DateTime;
/// use harness_tools_datetime::resolve;
///
/// let now = DateTime::parse_from_rfc3339("2026-06-15T09:00:00+08:00").unwrap();
/// let r = resolve("明天下午3点开会", now).unwrap();
/// assert_eq!(r.start.to_rfc3339(), "2026-06-16T15:00:00+08:00");
/// assert!(!r.date_only);
///
/// // Ambiguous input is refused, not guessed:
/// assert!(resolve("三点叫我", now).is_none());
/// ```
pub fn resolve(text: &str, now: DateTime<FixedOffset>) -> Option<Resolution> {
    resolve_with(text, now, &Conventions::default())
}

/// [`resolve`] with a caller-supplied [`Conventions`] table.
pub fn resolve_with(
    text: &str,
    now: DateTime<FixedOffset>,
    conventions: &Conventions,
) -> Option<Resolution> {
    parser::resolve_impl(text, now, conventions)
}
