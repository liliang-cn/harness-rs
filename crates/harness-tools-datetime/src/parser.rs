//! The deterministic scanner behind [`crate::resolve`].
//!
//! Design: four left-to-right passes over the char sequence — recurrence,
//! relative duration, date, time — each consuming spans so later passes can't
//! re-read them. Before any pass, a *poison* check rejects utterances that
//! contain time semantics the rules can't represent (interval recurrence,
//! "N times a day", weekend/holiday policies); after all passes, a *leftover*
//! check rejects inputs where an unconsumed clock expression remains. Both
//! guards trade recall for honesty: `None` means "hand this to a smarter
//! parser", never "probably X".

use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, NaiveTime, Timelike};

use crate::{Conventions, Recurrence, Resolution};

// ───── char-level helpers ────────────────────────────────────────────────

/// If `pat` occurs at `cs[i..]`, returns its length in chars.
fn starts(cs: &[char], i: usize, pat: &str) -> Option<usize> {
    let mut j = i;
    for pc in pat.chars() {
        if cs.get(j) != Some(&pc) {
            return None;
        }
        j += 1;
    }
    Some(j - i)
}

fn word_start(cs: &[char], i: usize) -> bool {
    i == 0 || !cs[i - 1].is_ascii_alphanumeric()
}

fn word_end(cs: &[char], end: usize) -> bool {
    end >= cs.len() || !cs[end].is_ascii_alphanumeric()
}

fn ch_digit(c: char) -> Option<u32> {
    Some(match c {
        '零' => 0,
        '一' => 1,
        '二' | '两' => 2,
        '三' => 3,
        '四' => 4,
        '五' => 5,
        '六' => 6,
        '七' => 7,
        '八' => 8,
        '九' => 9,
        _ => return None,
    })
}

/// Parse an Arabic (≤4 digits) or Chinese (0–99: X / 十 / 十X / X十 / X十Y)
/// number at `i`. Returns (value, chars consumed).
fn num_at(cs: &[char], i: usize) -> Option<(u32, usize)> {
    if cs.get(i).is_some_and(|c| c.is_ascii_digit()) {
        let mut v: u32 = 0;
        let mut l = 0;
        while l < 4 && cs.get(i + l).is_some_and(|c| c.is_ascii_digit()) {
            v = v * 10 + cs[i + l].to_digit(10).expect("ascii digit");
            l += 1;
        }
        return Some((v, l));
    }
    if cs.get(i) == Some(&'十') {
        if let Some(d) = cs.get(i + 1).copied().and_then(ch_digit).filter(|&d| d > 0) {
            return Some((10 + d, 2));
        }
        return Some((10, 1));
    }
    let x = cs.get(i).copied().and_then(ch_digit)?;
    if cs.get(i + 1) == Some(&'十') {
        if let Some(y) = cs.get(i + 2).copied().and_then(ch_digit).filter(|&d| d > 0) {
            return Some((x * 10 + y, 3));
        }
        return Some((x * 10, 2));
    }
    Some((x, 1))
}

/// A weekday char after 周/星期/礼拜 or in a 每周 day list. ISO 1–7.
fn wd_char(c: char) -> Option<u8> {
    Some(match c {
        '一' | '1' => 1,
        '二' | '2' => 2,
        '三' | '3' => 3,
        '四' | '4' => 4,
        '五' | '5' => 5,
        '六' | '6' => 6,
        '日' | '天' | '7' => 7,
        _ => return None,
    })
}

const EN_WEEKDAYS: &[(&str, u8)] = &[
    ("monday", 1),
    ("tuesday", 2),
    ("wednesday", 3),
    ("thursday", 4),
    ("friday", 5),
    ("saturday", 6),
    ("sunday", 7),
];

// ───── meridiem ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mer {
    Dawn,
    Am,
    Noon,
    Pm,
    Night,
}

const MERS: &[(&str, Mer)] = &[
    ("凌晨", Mer::Dawn),
    ("清晨", Mer::Am),
    ("早晨", Mer::Am),
    ("早上", Mer::Am),
    ("上午", Mer::Am),
    ("中午", Mer::Noon),
    ("正午", Mer::Noon),
    ("下午", Mer::Pm),
    ("午后", Mer::Pm),
    ("傍晚", Mer::Night),
    ("晚上", Mer::Night),
    ("晚间", Mer::Night),
    ("夜里", Mer::Night),
];

/// Map a written hour through its meridiem to (hour 0–23, day carry).
/// 晚上12点 is midnight at the end of today → (0, carry 1). Combinations that
/// make no sense (上午15点) are rejected.
fn apply_mer(mer: Mer, h: u32) -> Option<(u32, i64)> {
    Some(match mer {
        Mer::Dawn => match h {
            12 => (0, 0),
            0..=11 => (h, 0),
            _ => return None,
        },
        Mer::Am => match h {
            0..=12 => (h, 0),
            _ => return None,
        },
        Mer::Noon => match h {
            11 | 12 => (h, 0),
            1..=3 => (h + 12, 0),
            _ => return None,
        },
        Mer::Pm => match h {
            12 => (12, 0),
            1..=11 => (h + 12, 0),
            _ => return None,
        },
        Mer::Night => match h {
            12 => (0, 1),
            1..=11 => (h + 12, 0),
            13..=23 => (h, 0),
            _ => return None,
        },
    })
}

/// A resolved time-of-day. `carry` shifts the date (晚上12点 → next 00:00).
#[derive(Clone, Copy, Debug)]
struct Tp {
    h: u32,
    m: u32,
    carry: i64,
}

// ───── clock forms ───────────────────────────────────────────────────────

/// `N点[半|一刻|三刻|整|钟|N[分]]` or `H:MM` / `H：MM` at `i`.
/// Returns (hour as written, minute, len, is_colon_form).
fn clock_at(cs: &[char], i: usize) -> Option<(u32, u32, usize, bool)> {
    let (h, hl) = num_at(cs, i)?;
    if matches!(cs.get(i + hl), Some(&':') | Some(&'：')) {
        let (m, ml) = num_at(cs, i + hl + 1)?;
        if h <= 23 && m <= 59 && ml <= 2 {
            return Some((h, m, hl + 1 + ml, true));
        }
        return None;
    }
    if cs.get(i + hl) == Some(&'点') {
        if h > 23 {
            return None;
        }
        let mut j = i + hl + 1;
        let mut m = 0;
        if let Some(l) = starts(cs, j, "半") {
            m = 30;
            j += l;
        } else if let Some(l) = starts(cs, j, "一刻") {
            m = 15;
            j += l;
        } else if let Some(l) = starts(cs, j, "三刻") {
            m = 45;
            j += l;
        } else if let Some(l) = starts(cs, j, "整").or_else(|| starts(cs, j, "钟")) {
            j += l;
        } else if let Some((n, l)) = num_at(cs, j)
            && n <= 59
        {
            m = n;
            j += l;
            if let Some(l2) = starts(cs, j, "分") {
                j += l2;
            }
        }
        return Some((h, m, j - i, false));
    }
    None
}

/// English clock at `i` (on the ascii-lowercased text): optional `at `,
/// `H[:MM]` + `am`/`pm`, or `noon` / `midnight`. Returns (hour 0–23, minute, len).
fn en_time_at(lc: &[char], i: usize) -> Option<(u32, u32, usize)> {
    if !word_start(lc, i) {
        return None;
    }
    if let Some(l) = starts(lc, i, "noon")
        && word_end(lc, i + l)
    {
        return Some((12, 0, l));
    }
    if let Some(l) = starts(lc, i, "midnight")
        && word_end(lc, i + l)
    {
        return Some((0, 0, l));
    }
    let mut j = i;
    if let Some(l) = starts(lc, j, "at ") {
        j += l;
    }
    let (h, hl) = num_at(lc, j)?;
    if !(1..=12).contains(&h) || hl > 2 {
        return None;
    }
    let mut k = j + hl;
    let mut m = 0;
    if lc.get(k) == Some(&':') {
        let (mm, ml) = num_at(lc, k + 1)?;
        if mm > 59 || ml > 2 {
            return None;
        }
        m = mm;
        k += 1 + ml;
    }
    if lc.get(k) == Some(&' ') {
        k += 1;
    }
    let pm = if let Some(l) = starts(lc, k, "p.m.").or_else(|| starts(lc, k, "pm")) {
        k += l;
        true
    } else {
        let l = starts(lc, k, "a.m.").or_else(|| starts(lc, k, "am"))?;
        k += l;
        false
    };
    if !word_end(lc, k) {
        return None;
    }
    let h24 = if pm { h % 12 + 12 } else { h % 12 };
    Some((h24, m, k - i))
}

// ───── relative durations ────────────────────────────────────────────────

fn rel_at(cs: &[char], lc: &[char], i: usize) -> Option<(Duration, usize)> {
    // 半[个]小时后 — but not "一个半小时后" (1.5h is out of scope, not 30m).
    for pat in ["半个小时后", "半个钟头后", "半小时之后", "半小时后"] {
        if let Some(l) = starts(cs, i, pat) {
            let prev_bad = i > 0
                && (cs[i - 1] == '个'
                    || ch_digit(cs[i - 1]).is_some()
                    || cs[i - 1].is_ascii_digit());
            if !prev_bad {
                return Some((Duration::minutes(30), l));
            }
        }
    }
    if let Some(l) = starts(cs, i, "一刻钟后") {
        return Some((Duration::minutes(15), l));
    }
    // N[个](小时|钟头)[之]后 / N分钟[之]后
    if let Some((n, nl)) = num_at(cs, i) {
        let mut j = i + nl;
        if let Some(l) = starts(cs, j, "个") {
            j += l;
        }
        let unit = if let Some(l) = starts(cs, j, "小时").or_else(|| starts(cs, j, "钟头")) {
            Some((Duration::hours(i64::from(n)), j + l))
        } else {
            starts(cs, j, "分钟").map(|l| (Duration::minutes(i64::from(n)), j + l))
        };
        if let Some((d, mut k)) = unit {
            if let Some(l) = starts(cs, k, "之") {
                k += l;
            }
            if let Some(l) = starts(cs, k, "后") {
                return Some((d, k + l - i));
            }
        }
    }
    if word_start(lc, i) {
        if let Some(l) = starts(lc, i, "in half an hour")
            && word_end(lc, i + l)
        {
            return Some((Duration::minutes(30), l));
        }
        if let Some(l) = starts(lc, i, "in an hour")
            && word_end(lc, i + l)
            && starts(lc, i + l, " and").is_none()
        {
            return Some((Duration::hours(1), l));
        }
        if let Some(l) = starts(lc, i, "in ")
            && let Some((n, nl)) = num_at(lc, i + l)
        {
            let j = i + l + nl;
            for (unit, is_min) in [
                (" minutes", true),
                (" minute", true),
                (" mins", true),
                (" min", true),
                (" hours", false),
                (" hour", false),
                (" hrs", false),
                (" hr", false),
            ] {
                if let Some(ul) = starts(lc, j, unit)
                    && word_end(lc, j + ul)
                {
                    let d = if is_min {
                        Duration::minutes(i64::from(n))
                    } else {
                        Duration::hours(i64::from(n))
                    };
                    return Some((d, j + ul - i));
                }
            }
        }
    }
    None
}

// ───── dates ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Which {
    /// Bare 周X / friday: nearest future occurrence.
    Coming,
    /// 这周X / this friday: current Monday-started week, even if past.
    This,
    /// 下周X (1) / 下下周X (2) / next friday (1).
    Next(i64),
}

#[derive(Clone, Debug)]
enum DateSpec {
    /// Days from today: 今天 0, 明天 1, …, 3天后, in 2 weeks.
    Offset(i64),
    WeekdayRef {
        wd: u8,
        which: Which,
    },
    MonthDay {
        year: Option<i32>,
        month: u32,
        day: u32,
    },
}

/// `[YYYY年]M月D日|号` (Arabic or Chinese numerals).
fn month_day_at(cs: &[char], i: usize) -> Option<(DateSpec, usize)> {
    let mut j = i;
    let mut year = None;
    if let Some((y, yl)) = num_at(cs, j)
        && cs.get(j + yl) == Some(&'年')
        && (1970..=9999).contains(&y)
    {
        year = Some(y as i32);
        j += yl + 1;
    }
    let (m, ml) = num_at(cs, j)?;
    if cs.get(j + ml) != Some(&'月') || !(1..=12).contains(&m) {
        return None;
    }
    let k = j + ml + 1;
    let (d, dl) = num_at(cs, k)?;
    if !(1..=31).contains(&d) {
        return None;
    }
    let e = k + dl;
    let el = starts(cs, e, "日").or_else(|| starts(cs, e, "号"))?;
    Some((
        DateSpec::MonthDay {
            year,
            month: m,
            day: d,
        },
        e + el - i,
    ))
}

/// Returns (spec, len, pending meridiem carried by the date word — 今晚 both
/// names a day and promises a Night reading for a bare clock that follows).
fn date_at(cs: &[char], lc: &[char], i: usize) -> Option<(DateSpec, usize, Option<Mer>)> {
    for (pat, off, mer) in [
        ("大后天", 3, None),
        ("后天", 2, None),
        ("明天", 1, None),
        ("明日", 1, None),
        ("今天", 0, None),
        ("今晚", 0, Some(Mer::Night)),
        ("明晚", 1, Some(Mer::Night)),
        ("今早", 0, Some(Mer::Am)),
        ("明早", 1, Some(Mer::Am)),
    ] {
        if let Some(l) = starts(cs, i, pat) {
            return Some((DateSpec::Offset(off), l, mer));
        }
    }
    // N天后 / N[个]周后 / N[个]星期后
    if let Some((n, nl)) = num_at(cs, i) {
        let j = i + nl;
        if let Some(l) = starts(cs, j, "天之后").or_else(|| starts(cs, j, "天后")) {
            return Some((DateSpec::Offset(i64::from(n)), j + l - i, None));
        }
        let mut k = j;
        if let Some(l) = starts(cs, k, "个") {
            k += l;
        }
        for pat in ["星期之后", "星期后", "周之后", "周后"] {
            if let Some(l) = starts(cs, k, pat) {
                return Some((DateSpec::Offset(i64::from(n) * 7), k + l - i, None));
            }
        }
    }
    if let Some((spec, l)) = month_day_at(cs, i) {
        return Some((spec, l, None));
    }
    // (下下|下|这|本)?(周|星期|礼拜)X
    {
        let (which, pl) = if let Some(l) = starts(cs, i, "下下") {
            (Which::Next(2), l)
        } else if let Some(l) = starts(cs, i, "下") {
            (Which::Next(1), l)
        } else if let Some(l) = starts(cs, i, "这") {
            (Which::This, l)
        } else if let Some(l) = starts(cs, i, "本") {
            (Which::This, l)
        } else {
            (Which::Coming, 0)
        };
        let j = i + pl;
        if let Some(wl) = starts(cs, j, "星期")
            .or_else(|| starts(cs, j, "礼拜"))
            .or_else(|| starts(cs, j, "周"))
            && let Some(wd) = cs.get(j + wl).copied().and_then(wd_char)
        {
            return Some((DateSpec::WeekdayRef { wd, which }, j + wl + 1 - i, None));
        }
    }
    // English
    if word_start(lc, i) {
        for (pat, off, mer) in [
            ("the day after tomorrow", 2, None),
            ("day after tomorrow", 2, None),
            ("tomorrow", 1, None),
            ("tonight", 0, Some(Mer::Night)),
            ("today", 0, None),
        ] {
            if let Some(l) = starts(lc, i, pat)
                && word_end(lc, i + l)
            {
                return Some((DateSpec::Offset(off), l, mer));
            }
        }
        if let Some(l) = starts(lc, i, "in a week")
            && word_end(lc, i + l)
        {
            return Some((DateSpec::Offset(7), l, None));
        }
        if let Some(l) = starts(lc, i, "in ")
            && let Some((n, nl)) = num_at(lc, i + l)
        {
            let j = i + l + nl;
            for (unit, mult) in [(" days", 1), (" day", 1), (" weeks", 7), (" week", 7)] {
                if let Some(ul) = starts(lc, j, unit)
                    && word_end(lc, j + ul)
                {
                    return Some((DateSpec::Offset(i64::from(n) * mult), j + ul - i, None));
                }
            }
        }
        let (which, pl) = if let Some(l) = starts(lc, i, "next ") {
            (Which::Next(1), l)
        } else if let Some(l) = starts(lc, i, "this ") {
            (Which::This, l)
        } else {
            (Which::Coming, 0)
        };
        let j = i + pl;
        for &(name, wd) in EN_WEEKDAYS {
            if let Some(l) = starts(lc, j, name)
                && word_end(lc, j + l)
            {
                return Some((DateSpec::WeekdayRef { wd, which }, j + l - i, None));
            }
        }
    }
    None
}

// ───── recurrences ───────────────────────────────────────────────────────

fn rec_at(cs: &[char], lc: &[char], i: usize) -> Option<(Recurrence, usize)> {
    if let Some(l) = starts(cs, i, "每天").or_else(|| starts(cs, i, "每日")) {
        return Some((Recurrence::Daily, l));
    }
    if let Some(l) = starts(cs, i, "工作日") {
        return Some((
            Recurrence::Weekly {
                days: vec![1, 2, 3, 4, 5],
            },
            l,
        ));
    }
    for p in ["每个月", "每月"] {
        if let Some(l) = starts(cs, i, p)
            && let Some((d, dl)) = num_at(cs, i + l)
            && (1..=31).contains(&d)
        {
            let e = i + l + dl;
            if let Some(el) = starts(cs, e, "号").or_else(|| starts(cs, e, "日")) {
                return Some((Recurrence::Monthly { day: d }, e + el - i));
            }
        }
    }
    if let Some(l) = starts(cs, i, "每年")
        && let Some((m, ml)) = num_at(cs, i + l)
        && (1..=12).contains(&m)
        && cs.get(i + l + ml) == Some(&'月')
    {
        let k = i + l + ml + 1;
        if let Some((d, dl)) = num_at(cs, k)
            && (1..=31).contains(&d)
        {
            let e = k + dl;
            if let Some(el) = starts(cs, e, "日").or_else(|| starts(cs, e, "号")) {
                return Some((Recurrence::Yearly { month: m, day: d }, e + el - i));
            }
        }
    }
    for p in ["每个星期", "每星期", "每周"] {
        if let Some(l) = starts(cs, i, p) {
            let mut days = Vec::new();
            let mut j = i + l;
            while let Some(wd) = cs.get(j).copied().and_then(wd_char) {
                days.push(wd);
                j += 1;
            }
            // "每周一次" means once a week, not Monday — refuse.
            if !days.is_empty() && cs.get(j) != Some(&'次') {
                days.sort_unstable();
                days.dedup();
                return Some((Recurrence::Weekly { days }, j - i));
            }
        }
    }
    if word_start(lc, i) {
        for pat in ["every day", "everyday", "daily"] {
            if let Some(l) = starts(lc, i, pat)
                && word_end(lc, i + l)
            {
                return Some((Recurrence::Daily, l));
            }
        }
        for pat in ["every weekday", "on weekdays", "weekdays"] {
            if let Some(l) = starts(lc, i, pat)
                && word_end(lc, i + l)
            {
                return Some((
                    Recurrence::Weekly {
                        days: vec![1, 2, 3, 4, 5],
                    },
                    l,
                ));
            }
        }
        if let Some(l) = starts(lc, i, "every ") {
            for &(name, wd) in EN_WEEKDAYS {
                if let Some(nl) = starts(lc, i + l, name)
                    && word_end(lc, i + l + nl)
                {
                    return Some((Recurrence::Weekly { days: vec![wd] }, l + nl));
                }
            }
        }
    }
    None
}

// ───── guards ────────────────────────────────────────────────────────────

/// Utterances whose time semantics the rules cannot represent. Matching any
/// of these rejects the whole input — a partial parse would silently drop
/// the part that mattered.
fn poisoned(cs: &[char], lc: &[char]) -> bool {
    for pat in ["每隔", "早中晚", "周末", "节假日", "假期"] {
        if (0..cs.len()).any(|i| starts(cs, i, pat).is_some()) {
            return true;
        }
    }
    for pat in ["weekend", "holiday", "hourly", "every hour", "every other"] {
        if (0..lc.len()).any(|i| starts(lc, i, pat).is_some()) {
            return true;
        }
    }
    for i in 0..cs.len() {
        // 每[个|N]*(小时|分钟|钟头): interval recurrence.
        if cs[i] == '每' {
            let mut j = i + 1;
            for _ in 0..3 {
                if cs.get(j) == Some(&'个') {
                    j += 1;
                } else if let Some((_, l)) = num_at(cs, j) {
                    j += l;
                } else {
                    break;
                }
            }
            for unit in ["小时", "分钟", "钟头"] {
                if starts(cs, j, unit).is_some() {
                    return true;
                }
            }
        }
        // N次: "一天三次", "每周一次" — count semantics, not clock semantics.
        if let Some((_, l)) = num_at(cs, i)
            && cs.get(i + l) == Some(&'次')
        {
            return true;
        }
        // every N hours / minutes
        if let Some(l) = starts(lc, i, "every ")
            && let Some((_, nl)) = num_at(lc, i + l)
        {
            let j = i + l + nl;
            for unit in [" hour", " minute", " time"] {
                if starts(lc, j, unit).is_some() {
                    return true;
                }
            }
        }
    }
    false
}

fn span_free(consumed: &[(usize, usize)], s: usize, e: usize) -> bool {
    !consumed.iter().any(|&(a, b)| s < b && a < e)
}

/// After all passes: does unconsumed text still contain clock evidence
/// (`N点`, `H:MM`, `at N`)? If so the parse missed something — refuse.
fn leftover_time_evidence(cs: &[char], lc: &[char], consumed: &[(usize, usize)]) -> bool {
    for i in 0..cs.len() {
        if let Some((_, l)) = num_at(cs, i)
            && span_free(consumed, i, i + l + 1)
        {
            match cs.get(i + l) {
                Some(&'点') => return true,
                Some(&':') | Some(&'：') if num_at(cs, i + l + 1).is_some() => return true,
                _ => {}
            }
        }
        if word_start(lc, i)
            && let Some(l) = starts(lc, i, "at ")
            && num_at(lc, i + l).is_some()
            && span_free(consumed, i, i + l + 1)
        {
            return true;
        }
    }
    false
}

// ───── time pass ─────────────────────────────────────────────────────────

fn time_at(
    cs: &[char],
    lc: &[char],
    i: usize,
    pending: Option<Mer>,
    conv: &Conventions,
) -> Option<(Tp, usize)> {
    for (phrase, t) in &conv.phrases {
        if let Some(l) = starts(cs, i, phrase) {
            return Some((
                Tp {
                    h: t.hour(),
                    m: t.minute(),
                    carry: 0,
                },
                l,
            ));
        }
    }
    if let Some((h, m, l)) = en_time_at(lc, i) {
        return Some((Tp { h, m, carry: 0 }, l));
    }
    for &(word, mer) in MERS {
        if let Some(wl) = starts(cs, i, word) {
            if let Some((h, m, cl, _)) = clock_at(cs, i + wl) {
                if let Some((h24, carry)) = apply_mer(mer, h) {
                    return Some((Tp { h: h24, m, carry }, wl + cl));
                }
            } else if mer == Mer::Noon {
                // Bare 中午/正午 carries its own clock time.
                return Some((
                    Tp {
                        h: 12,
                        m: 0,
                        carry: 0,
                    },
                    wl,
                ));
            }
        }
    }
    // Bare clock. Reject when glued to a preceding ASCII digit (mid-number).
    if i > 0 && cs[i - 1].is_ascii_digit() {
        return None;
    }
    if let Some((h, m, l, colon)) = clock_at(cs, i) {
        if let Some(mer) = pending
            && h <= 12
            && let Some((h24, carry)) = apply_mer(mer, h)
        {
            return Some((Tp { h: h24, m, carry }, l));
        }
        if colon {
            // H:MM reads as a literal 24-hour clock time.
            return Some((Tp { h, m, carry: 0 }, l));
        }
        if (13..=23).contains(&h) || h == 0 {
            return Some((Tp { h, m, carry: 0 }, l));
        }
        // 1..=12点 without a meridiem: ambiguous — refuse to guess.
    }
    None
}

// ───── calendar arithmetic ───────────────────────────────────────────────

fn dt_of(date: NaiveDate, tp: Option<&Tp>, off: FixedOffset) -> Option<DateTime<FixedOffset>> {
    let (d, t) = match tp {
        None => (date, NaiveTime::MIN),
        Some(tp) => (
            date.checked_add_signed(Duration::days(tp.carry))?,
            NaiveTime::from_hms_opt(tp.h, tp.m, 0)?,
        ),
    };
    d.and_time(t).and_local_timezone(off).single()
}

fn monday_of(d: NaiveDate) -> Option<NaiveDate> {
    d.checked_sub_signed(Duration::days(
        i64::from(d.weekday().number_from_monday()) - 1,
    ))
}

fn resolve_date(spec: &DateSpec, now: DateTime<FixedOffset>, tp: Option<&Tp>) -> Option<NaiveDate> {
    let off = *now.offset();
    let today = now.date_naive();
    match spec {
        DateSpec::Offset(n) => today.checked_add_signed(Duration::days(*n)),
        DateSpec::WeekdayRef { wd, which } => {
            let cur = i64::from(now.weekday().number_from_monday());
            match which {
                Which::Coming => {
                    let mut delta = (i64::from(*wd) + 7 - cur) % 7;
                    if delta == 0 {
                        let future = match tp {
                            None => true,
                            Some(t) => dt_of(today, Some(t), off)? > now,
                        };
                        if !future {
                            delta = 7;
                        }
                    }
                    today.checked_add_signed(Duration::days(delta))
                }
                Which::This => {
                    monday_of(today)?.checked_add_signed(Duration::days(i64::from(*wd) - 1))
                }
                Which::Next(k) => {
                    monday_of(today)?.checked_add_signed(Duration::days(7 * k + i64::from(*wd) - 1))
                }
            }
        }
        DateSpec::MonthDay {
            year: Some(y),
            month,
            day,
        } => NaiveDate::from_ymd_opt(*y, *month, *day),
        DateSpec::MonthDay {
            year: None,
            month,
            day,
        } => {
            // Next occurrence; skips years where the date doesn't exist (2月29).
            for y in today.year()..today.year() + 9 {
                if let Some(date) = NaiveDate::from_ymd_opt(y, *month, *day) {
                    if date > today {
                        return Some(date);
                    }
                    if date == today {
                        let ok = match tp {
                            None => true,
                            Some(t) => dt_of(date, Some(t), off)? > now,
                        };
                        if ok {
                            return Some(date);
                        }
                    }
                }
            }
            None
        }
    }
}

fn rec_first(
    rec: &Recurrence,
    now: DateTime<FixedOffset>,
    tp: Option<&Tp>,
) -> Option<DateTime<FixedOffset>> {
    let off = *now.offset();
    let today = now.date_naive();
    match rec {
        Recurrence::Daily => match tp {
            None => dt_of(today, None, off),
            Some(t) => {
                let dt = dt_of(today, Some(t), off)?;
                if dt > now {
                    Some(dt)
                } else {
                    dt_of(today.succ_opt()?, Some(t), off)
                }
            }
        },
        Recurrence::Weekly { days } => {
            let cur = i64::from(now.weekday().number_from_monday());
            let mut best: Option<DateTime<FixedOffset>> = None;
            for &d in days {
                let delta = (i64::from(d) + 7 - cur) % 7;
                let date = today.checked_add_signed(Duration::days(delta))?;
                let mut dt = dt_of(date, tp, off)?;
                if tp.is_some() && dt <= now {
                    dt = dt_of(date.checked_add_signed(Duration::days(7))?, tp, off)?;
                }
                best = Some(match best {
                    None => dt,
                    Some(b) => b.min(dt),
                });
            }
            best
        }
        Recurrence::Monthly { day } => {
            let (mut y, mut m) = (today.year(), today.month());
            for _ in 0..48 {
                if let Some(date) = NaiveDate::from_ymd_opt(y, m, *day)
                    && date >= today
                {
                    let dt = dt_of(date, tp, off)?;
                    if date > today || tp.is_none() || dt > now {
                        return Some(dt);
                    }
                }
                m += 1;
                if m > 12 {
                    m = 1;
                    y += 1;
                }
            }
            None
        }
        Recurrence::Yearly { month, day } => {
            for y in today.year()..today.year() + 9 {
                if let Some(date) = NaiveDate::from_ymd_opt(y, *month, *day)
                    && date >= today
                {
                    let dt = dt_of(date, tp, off)?;
                    if date > today || tp.is_none() || dt > now {
                        return Some(dt);
                    }
                }
            }
            None
        }
    }
}

// ───── main entry ────────────────────────────────────────────────────────

pub(crate) fn resolve_impl(
    text: &str,
    now: DateTime<FixedOffset>,
    conv: &Conventions,
) -> Option<Resolution> {
    let cs: Vec<char> = text.chars().collect();
    let lc: Vec<char> = cs.iter().map(|c| c.to_ascii_lowercase()).collect();
    if poisoned(&cs, &lc) {
        return None;
    }

    let mut consumed: Vec<(usize, usize)> = Vec::new();
    let mut frags: Vec<(usize, usize)> = Vec::new();

    // Pass 1: recurrence (must run before dates so 每周一 isn't read as 周一).
    let mut rec: Option<Recurrence> = None;
    let mut i = 0;
    while i < cs.len() {
        if let Some((r, l)) = rec_at(&cs, &lc, i)
            && span_free(&consumed, i, i + l)
        {
            if rec.is_some() {
                return None; // two recurrence rules in one utterance
            }
            rec = Some(r);
            consumed.push((i, i + l));
            frags.push((i, i + l));
            i += l;
            continue;
        }
        i += 1;
    }

    // Pass 2: relative durations (半小时后, in 30 minutes).
    let mut rel: Option<Duration> = None;
    i = 0;
    while i < cs.len() {
        if let Some((d, l)) = rel_at(&cs, &lc, i)
            && span_free(&consumed, i, i + l)
        {
            if rel.is_some() {
                return None;
            }
            rel = Some(d);
            consumed.push((i, i + l));
            frags.push((i, i + l));
            i += l;
            continue;
        }
        i += 1;
    }

    // Pass 3: date.
    let mut date: Option<DateSpec> = None;
    let mut pending: Option<Mer> = None;
    i = 0;
    while i < cs.len() {
        if let Some((spec, l, mer)) = date_at(&cs, &lc, i)
            && span_free(&consumed, i, i + l)
        {
            if date.is_some() {
                return None; // "明天或后天" — don't pick one
            }
            date = Some(spec);
            pending = mer;
            consumed.push((i, i + l));
            frags.push((i, i + l));
            i += l;
            continue;
        }
        i += 1;
    }

    // Pass 4: time-of-day. A second match means a range or an enumeration —
    // out of scope, refuse.
    let mut time: Option<Tp> = None;
    i = 0;
    while i < cs.len() {
        if let Some((tp, l)) = time_at(&cs, &lc, i, pending, conv)
            && span_free(&consumed, i, i + l)
        {
            if time.is_some() {
                return None;
            }
            time = Some(tp);
            consumed.push((i, i + l));
            frags.push((i, i + l));
            i += l;
            continue;
        }
        i += 1;
    }

    if leftover_time_evidence(&cs, &lc, &consumed) {
        return None;
    }

    frags.sort_unstable();
    let matched: Vec<String> = frags
        .iter()
        .map(|&(s, e)| cs[s..e].iter().collect())
        .collect();

    let off = *now.offset();
    if let Some(d) = rel {
        // "半小时后" mixed with a date, a clock time or a recurrence is not a
        // pattern we understand — refuse rather than pick a winner.
        if rec.is_some() || date.is_some() || time.is_some() {
            return None;
        }
        return Some(Resolution {
            start: now + d,
            date_only: false,
            recurrence: None,
            matched,
        });
    }
    if let Some(r) = rec {
        if date.is_some() {
            return None;
        }
        let start = rec_first(&r, now, time.as_ref())?;
        return Some(Resolution {
            start,
            date_only: time.is_none(),
            recurrence: Some(r),
            matched,
        });
    }
    match (date, time) {
        (None, None) => None,
        (Some(spec), None) => {
            let d = resolve_date(&spec, now, None)?;
            Some(Resolution {
                start: dt_of(d, None, off)?,
                date_only: true,
                recurrence: None,
                matched,
            })
        }
        (None, Some(tp)) => {
            // Time-only: the next occurrence.
            let today = now.date_naive();
            let mut dt = dt_of(today, Some(&tp), off)?;
            if dt <= now {
                dt = dt_of(today.succ_opt()?, Some(&tp), off)?;
            }
            Some(Resolution {
                start: dt,
                date_only: false,
                recurrence: None,
                matched,
            })
        }
        (Some(spec), Some(tp)) => {
            let d = resolve_date(&spec, now, Some(&tp))?;
            Some(Resolution {
                start: dt_of(d, Some(&tp), off)?,
                date_only: false,
                recurrence: None,
                matched,
            })
        }
    }
}
