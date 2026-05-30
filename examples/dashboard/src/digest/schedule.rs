//! Pure scheduling predicate for the digest cron. Kept free of I/O so it can
//! be exhaustively unit-tested.

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;

/// Parse "HH:MM" into a `NaiveTime`. Invalid input falls back to 08:00.
pub fn parse_send_time(s: &str) -> NaiveTime {
    let mut parts = s.splitn(2, ':');
    let h: u32 = parts
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(8);
    let m: u32 = parts
        .next()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0);
    NaiveTime::from_hms_opt(h.min(23), m.min(59), 0)
        .unwrap_or_else(|| NaiveTime::from_hms_opt(8, 0, 0).unwrap())
}

/// Parse an IANA timezone, falling back to UTC (logging the bad value).
pub fn parse_tz(s: &str) -> Tz {
    match s.parse::<Tz>() {
        Ok(tz) => tz,
        Err(_) => {
            tracing::warn!(tz = %s, "bad digest timezone, falling back to UTC");
            Tz::UTC
        }
    }
}

/// True when a digest should be sent right now for this user:
/// it's at/after their local send-time today AND they haven't been sent today.
pub fn is_due(
    now_utc: DateTime<Utc>,
    tz: Tz,
    send_time: &str,
    last_digest_date: Option<&str>,
) -> bool {
    let local = now_utc.with_timezone(&tz);
    let local_date = local.format("%Y-%m-%d").to_string();
    if last_digest_date == Some(local_date.as_str()) {
        return false; // already sent today (user-local)
    }
    local.time() >= parse_send_time(send_time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::Asia::Shanghai;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn fires_after_local_send_time() {
        assert!(is_due(utc(2026, 5, 29, 1, 0), Shanghai, "08:00", None));
    }

    #[test]
    fn not_yet_before_local_send_time() {
        assert!(!is_due(utc(2026, 5, 28, 23, 0), Shanghai, "08:00", None));
    }

    #[test]
    fn skips_if_already_sent_today_local() {
        assert!(!is_due(
            utc(2026, 5, 29, 1, 0),
            Shanghai,
            "08:00",
            Some("2026-05-29")
        ));
    }

    #[test]
    fn fires_new_local_day_even_if_sent_yesterday() {
        assert!(is_due(
            utc(2026, 5, 29, 1, 0),
            Shanghai,
            "08:00",
            Some("2026-05-28")
        ));
    }

    #[test]
    fn bad_tz_falls_back_to_utc() {
        assert!(is_due(
            utc(2026, 5, 29, 9, 0),
            parse_tz("Not/AZone"),
            "08:00",
            None
        ));
    }

    #[test]
    fn parse_send_time_handles_garbage() {
        assert_eq!(
            parse_send_time("7:5"),
            NaiveTime::from_hms_opt(7, 5, 0).unwrap()
        );
        assert_eq!(
            parse_send_time("nope"),
            NaiveTime::from_hms_opt(8, 0, 0).unwrap()
        );
        assert_eq!(
            parse_send_time("25:99"),
            NaiveTime::from_hms_opt(23, 59, 0).unwrap()
        );
    }
}
