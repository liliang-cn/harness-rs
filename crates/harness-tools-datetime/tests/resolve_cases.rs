//! Table-driven cases for `resolve`: one line per (input, now, expectation).
//!
//! A large slice of the corpus is ported from SmartAlarm's LLM parser eval
//! (`SmartAlarm/server/internal/parser/eval_test.go`, 30 labelled Chinese
//! voice-alarm utterances, anchored at the same "now": Monday 2026-06-15
//! 09:00 +08:00). Only the deterministic-parseable utterances are ported as
//! positive cases; the rest are asserted as `None` and documented as
//! out-of-scope below.

use chrono::{DateTime, FixedOffset};
use harness_tools_datetime::{Recurrence, resolve};

/// Monday morning. Same anchor as SmartAlarm's eval, so 下周三 / 工作日 are fixed.
const NOW: &str = "2026-06-15T09:00:00+08:00";

struct Case {
    text: &'static str,
    now: &'static str,
    /// (start RFC3339, date_only, recurrence); `None` = must refuse.
    want: Option<(&'static str, bool, Option<Recurrence>)>,
}

fn at(text: &'static str, start: &'static str) -> Case {
    Case {
        text,
        now: NOW,
        want: Some((start, false, None)),
    }
}
fn at_from(text: &'static str, now: &'static str, start: &'static str) -> Case {
    Case {
        text,
        now,
        want: Some((start, false, None)),
    }
}
fn day(text: &'static str, start: &'static str) -> Case {
    Case {
        text,
        now: NOW,
        want: Some((start, true, None)),
    }
}
fn day_from(text: &'static str, now: &'static str, start: &'static str) -> Case {
    Case {
        text,
        now,
        want: Some((start, true, None)),
    }
}
fn rec(text: &'static str, start: &'static str, date_only: bool, r: Recurrence) -> Case {
    Case {
        text,
        now: NOW,
        want: Some((start, date_only, Some(r))),
    }
}
fn none(text: &'static str) -> Case {
    Case {
        text,
        now: NOW,
        want: None,
    }
}

fn weekly(days: &[u8]) -> Recurrence {
    Recurrence::Weekly {
        days: days.to_vec(),
    }
}

fn cases() -> Vec<Case> {
    vec![
        // ── Chinese · one-shot date+time ─────────────────────────────────
        at("明天下午3点开会", "2026-06-16T15:00:00+08:00"),
        at("后天下午三点开会", "2026-06-17T15:00:00+08:00"), // SmartAlarm
        at("大后天早上六点四十叫醒我", "2026-06-18T06:40:00+08:00"), // SmartAlarm
        at("今天晚上8点", "2026-06-15T20:00:00+08:00"),
        at("今晚十点半提醒我睡觉", "2026-06-15T22:30:00+08:00"), // SmartAlarm
        at("明晚八点", "2026-06-16T20:00:00+08:00"),
        at("明早七点叫我", "2026-06-16T07:00:00+08:00"),
        at("三天后下午三点", "2026-06-18T15:00:00+08:00"),
        at("明天上午8点20分", "2026-06-16T08:20:00+08:00"),
        at("明天晚上八点一刻", "2026-06-16T20:15:00+08:00"),
        at("明天下午两点三刻", "2026-06-16T14:45:00+08:00"),
        // ── Chinese · weekdays ───────────────────────────────────────────
        at("周三下午2点", "2026-06-17T14:00:00+08:00"), // coming Wednesday
        // Bare 周一 at Mon 09:00 with 8:00 already past → next Monday.
        at("周一早上8点", "2026-06-22T08:00:00+08:00"),
        day("周一", "2026-06-15T00:00:00+08:00"), // date-only: today counts
        at("这周五晚上7点", "2026-06-19T19:00:00+08:00"),
        day("本周三", "2026-06-17T00:00:00+08:00"),
        at("下周三上午九点面试", "2026-06-24T09:00:00+08:00"), // SmartAlarm
        day("下周一", "2026-06-22T00:00:00+08:00"),
        day("下下周五", "2026-07-03T00:00:00+08:00"),
        at("礼拜天中午12点", "2026-06-21T12:00:00+08:00"),
        // ── Chinese · calendar dates ─────────────────────────────────────
        at("6月20号晚上8点提醒我交房租", "2026-06-20T20:00:00+08:00"), // SmartAlarm
        at("7月1日上午10点", "2026-07-01T10:00:00+08:00"),
        // 3月8日 has passed this year → next year.
        day("3月8号", "2027-03-08T00:00:00+08:00"),
        at("2026年12月31日23:59", "2026-12-31T23:59:00+08:00"),
        day("三月八号", "2027-03-08T00:00:00+08:00"), // Chinese-numeral month/day
        // ── Chinese · date-only relatives ────────────────────────────────
        day("明天", "2026-06-16T00:00:00+08:00"),
        day("3天后", "2026-06-18T00:00:00+08:00"),
        day("两周后", "2026-06-29T00:00:00+08:00"),
        // resolve() extracts time expressions; intent is the caller's job, so
        // even a weather question yields its date word.
        day("今天天气怎么样", "2026-06-15T00:00:00+08:00"), // SmartAlarm (unsupported branch there)
        // ── Chinese · time-only (next occurrence) ────────────────────────
        at("下午3点半", "2026-06-15T15:30:00+08:00"),
        at("下午三点半", "2026-06-15T15:30:00+08:00"),
        at("中午12点", "2026-06-15T12:00:00+08:00"),
        at("中午记得吃饭", "2026-06-15T12:00:00+08:00"), // SmartAlarm
        // 03:00 already past at 09:00 → tomorrow.
        at("凌晨三点", "2026-06-16T03:00:00+08:00"),
        // 晚上12点 = midnight at the end of today.
        at("晚上12点", "2026-06-16T00:00:00+08:00"),
        at("早上八点二十", "2026-06-16T08:20:00+08:00"), // 08:20 past → tomorrow
        at("8:20", "2026-06-16T08:20:00+08:00"),         // bare H:MM is 24h; past → tomorrow
        at("20:30", "2026-06-15T20:30:00+08:00"),
        at("23:59", "2026-06-15T23:59:00+08:00"),
        at("15点", "2026-06-15T15:00:00+08:00"), // ≥13 needs no meridiem
        // Extraction from modify-intent phrasing still works (SmartAlarm
        // routes these to a "modify" branch; here we only fix the time).
        at("把开会调到下午两点", "2026-06-15T14:00:00+08:00"), // SmartAlarm
        // 09:00 == now exactly → not in the future → tomorrow.
        at("把喝水调到上午9点", "2026-06-16T09:00:00+08:00"), // SmartAlarm
        // ── Chinese · relative durations ─────────────────────────────────
        at("半小时后叫我", "2026-06-15T09:30:00+08:00"), // SmartAlarm
        at("一刻钟后提醒我看炉子", "2026-06-15T09:15:00+08:00"), // SmartAlarm
        at("45分钟后", "2026-06-15T09:45:00+08:00"),
        at("2小时后", "2026-06-15T11:00:00+08:00"),
        at("两个小时后", "2026-06-15T11:00:00+08:00"),
        // ── Chinese · convention table (documented defaults) ─────────────
        at("下周五下班前", "2026-06-26T18:00:00+08:00"),
        at("睡前提醒我锁门", "2026-06-15T22:00:00+08:00"),
        // ── Chinese · recurrence ─────────────────────────────────────────
        rec(
            "每天晚上11点提醒我睡觉",
            "2026-06-15T23:00:00+08:00",
            false,
            Recurrence::Daily,
        ), // SmartAlarm
        rec(
            "每天早上7点",
            "2026-06-16T07:00:00+08:00",
            false,
            Recurrence::Daily,
        ), // 7:00 past today
        rec(
            "每天提醒我喝水",
            "2026-06-15T00:00:00+08:00",
            true,
            Recurrence::Daily,
        ),
        // Mon 7:00 already past → first hit is Wednesday.
        rec(
            "每周一三五早上7点起床",
            "2026-06-17T07:00:00+08:00",
            false,
            weekly(&[1, 3, 5]),
        ), // SmartAlarm
        rec(
            "每周日晚上8点",
            "2026-06-21T20:00:00+08:00",
            false,
            weekly(&[7]),
        ),
        // Today is the 15th and no time given → today, date-only.
        rec(
            "每个月15号提醒还信用卡",
            "2026-06-15T00:00:00+08:00",
            true,
            Recurrence::Monthly { day: 15 },
        ), // SmartAlarm
        rec(
            "每月1号早上9点",
            "2026-07-01T09:00:00+08:00",
            false,
            Recurrence::Monthly { day: 1 },
        ),
        // June has no 31st → July 31.
        rec(
            "每月31号",
            "2026-07-31T00:00:00+08:00",
            true,
            Recurrence::Monthly { day: 31 },
        ),
        // Mon 8:00 already past → Tuesday.
        rec(
            "工作日早上8点叫我上班",
            "2026-06-16T08:00:00+08:00",
            false,
            weekly(&[1, 2, 3, 4, 5]),
        ), // SmartAlarm
        // SmartAlarm's eval calls 每年 a hard case for the LLM; rules make it trivial.
        rec(
            "每年3月8号提醒我交年费",
            "2027-03-08T00:00:00+08:00",
            true,
            Recurrence::Yearly { month: 3, day: 8 },
        ), // SmartAlarm
        // Leap-day rule: next Feb 29 after 2026-06-15 is in 2028.
        rec(
            "每年2月29日",
            "2028-02-29T00:00:00+08:00",
            true,
            Recurrence::Yearly { month: 2, day: 29 },
        ),
        rec(
            "每天睡前提醒我吃药",
            "2026-06-15T22:00:00+08:00",
            false,
            Recurrence::Daily,
        ),
        // ── English ──────────────────────────────────────────────────────
        at("tomorrow at 3pm", "2026-06-16T15:00:00+08:00"),
        at("day after tomorrow at 9am", "2026-06-17T09:00:00+08:00"),
        at("tonight at 8pm", "2026-06-15T20:00:00+08:00"),
        at("next Friday at 6:30pm", "2026-06-26T18:30:00+08:00"),
        day("this friday", "2026-06-19T00:00:00+08:00"),
        at("Friday at noon", "2026-06-19T12:00:00+08:00"),
        day("in 3 days", "2026-06-18T00:00:00+08:00"),
        day("in 2 weeks", "2026-06-29T00:00:00+08:00"),
        day("tomorrow", "2026-06-16T00:00:00+08:00"),
        at("in 30 minutes", "2026-06-15T09:30:00+08:00"),
        at("in 2 hours", "2026-06-15T11:00:00+08:00"),
        at("in half an hour", "2026-06-15T09:30:00+08:00"),
        at("at 3pm", "2026-06-15T15:00:00+08:00"),
        at("at 15:00", "2026-06-15T15:00:00+08:00"),
        at("3:30pm", "2026-06-15T15:30:00+08:00"),
        at("midnight", "2026-06-16T00:00:00+08:00"), // 00:00 has passed → tomorrow
        rec(
            "every day at 10pm",
            "2026-06-15T22:00:00+08:00",
            false,
            Recurrence::Daily,
        ),
        // Mon 9:00 == now, not strictly future → next Monday.
        rec(
            "every Monday at 9am",
            "2026-06-22T09:00:00+08:00",
            false,
            weekly(&[1]),
        ),
        rec(
            "weekdays at 8am",
            "2026-06-16T08:00:00+08:00",
            false,
            weekly(&[1, 2, 3, 4, 5]),
        ),
        // ── boundaries: month/year crossings, 23:59, leap day ────────────
        at_from(
            "明天早上8点",
            "2026-01-31T10:00:00+08:00",
            "2026-02-01T08:00:00+08:00",
        ),
        day_from(
            "明天",
            "2026-12-31T23:00:00+08:00",
            "2027-01-01T00:00:00+08:00",
        ),
        at_from(
            "2小时后",
            "2026-12-31T23:30:00+08:00",
            "2027-01-01T01:30:00+08:00",
        ),
        at_from(
            "明天下午1点",
            "2024-02-28T09:00:00+08:00",
            "2024-02-29T13:00:00+08:00",
        ),
        at_from(
            "23:59",
            "2026-06-15T23:58:00+08:00",
            "2026-06-15T23:59:00+08:00",
        ),
        // 23:59:00 <= 23:59:30 → rolls to tomorrow.
        at_from(
            "23:59",
            "2026-06-15T23:59:30+08:00",
            "2026-06-16T23:59:00+08:00",
        ),
        // Timezone follows `now`, not the host.
        at_from(
            "tomorrow at 3pm",
            "2026-06-15T09:00:00-05:00",
            "2026-06-16T15:00:00-05:00",
        ),
        // ── out of scope: must refuse, never guess ───────────────────────
        // Bare 1–12 without a meridiem is ambiguous (SmartAlarm marks it
        // wantAmbiguity; we have no ambiguity channel, so: refuse).
        none("三点叫我"), // SmartAlarm
        // Interval recurrence (hourly/minutely) is unsupported.
        none("每两小时提醒我站起来活动"), // SmartAlarm
        none("每隔20分钟喝口水"),         // SmartAlarm
        none("every 2 hours"),
        // Holiday policies need a civil calendar we don't have.
        none("每天早上7点叫我，但节假日不要响"), // SmartAlarm
        // Time ranges and N-times-a-day enumerations.
        none("每天下午一点到六点每小时提醒我喝水"), // SmartAlarm
        none("提醒我吃药，每天早中晚三次"),         // SmartAlarm
        none("下午一点到六点开会"),
        // 周末 is fuzzy (Sat? Sun? both?) — refuse.
        none("周末早上9点叫我起床"), // SmartAlarm
        // No time expression at all.
        none("讲个笑话"),       // SmartAlarm
        none("谢谢你帮了大忙"), // SmartAlarm
        none("取消所有闹钟"),   // SmartAlarm (cancel intent, no time to fix)
        none("尽快提醒我"),
        none("晚饭后提醒我"),
        // Fractional durations are not silently truncated.
        none("一个半小时后"),
        // A leftover unparsed clock ("at 5") must poison the whole parse,
        // not yield a date-only "tomorrow".
        none("tomorrow at 5"),
        none("sometime next week"),
        // Two candidate dates — refuse rather than pick one.
        none("明天或后天提醒我"),
    ]
}

fn dt(s: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(s).unwrap()
}

#[test]
fn table() {
    for c in cases() {
        let got = resolve(c.text, dt(c.now));
        match (&c.want, &got) {
            (None, None) => {}
            (None, Some(r)) => panic!(
                "{:?} (now {}): expected None, got start={} date_only={} rec={:?} matched={:?}",
                c.text, c.now, r.start, r.date_only, r.recurrence, r.matched
            ),
            (Some(_), None) => panic!("{:?} (now {}): expected Some, got None", c.text, c.now),
            (Some((start, date_only, recur)), Some(r)) => {
                assert_eq!(
                    r.start,
                    dt(start),
                    "{:?} (now {}): start mismatch, got {} matched={:?}",
                    c.text,
                    c.now,
                    r.start,
                    r.matched
                );
                assert_eq!(
                    r.date_only, *date_only,
                    "{:?}: date_only mismatch (matched={:?})",
                    c.text, r.matched
                );
                assert_eq!(&r.recurrence, recur, "{:?}: recurrence mismatch", c.text);
                assert!(
                    !r.matched.is_empty(),
                    "{:?}: matched must not be empty",
                    c.text
                );
            }
        }
    }
}

#[test]
fn matched_fragments_are_in_text_order() {
    let r = resolve("明天下午3点开会", dt(NOW)).unwrap();
    assert_eq!(r.matched, vec!["明天".to_string(), "下午3点".to_string()]);
}

#[test]
fn custom_conventions_extend_the_table() {
    use chrono::NaiveTime;
    use harness_tools_datetime::{Conventions, resolve_with};
    let mut conv = Conventions::default();
    conv.phrases
        .push(("午休前".into(), NaiveTime::from_hms_opt(11, 45, 0).unwrap()));
    let r = resolve_with("明天午休前", dt(NOW), &conv).unwrap();
    assert_eq!(r.start, dt("2026-06-16T11:45:00+08:00"));
    assert!(!r.date_only);
    // Without the custom entry the phrase carries no clock evidence, so only
    // the date part resolves (date-only) — the caller sees via `matched` that
    // "午休前" was not consumed.
    let plain = resolve("明天午休前", dt(NOW)).unwrap();
    assert!(plain.date_only);
    assert_eq!(plain.matched, vec!["明天".to_string()]);
}
