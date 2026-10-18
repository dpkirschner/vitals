//! Timestamps. The export writes every date as local wall time with an explicit
//! offset (`2022-02-27 19:45:35 -0700`); we store epoch seconds **plus** that
//! offset so each sample's own local day is exact even across travel and DST
//! (invariant: local-day bucketing uses the sample's offset, never the
//! machine's timezone).

use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, TimeZone};

/// Parse an export date (`YYYY-MM-DD HH:MM:SS ±HHMM`) → (epoch secs, offset minutes).
pub fn parse_hk_date(s: &str) -> Option<(i64, i32)> {
    let dt = DateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S %z").ok()?;
    Some((dt.timestamp(), dt.offset().local_minus_utc() / 60))
}

/// Parse an RFC 3339 timestamp (GPX `<time>`) → epoch secs.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp())
}

/// The local calendar date of an instant, at the given offset.
pub fn local_date(ts: i64, offset_min: i32) -> NaiveDate {
    let off = FixedOffset::east_opt(offset_min * 60).unwrap_or(FixedOffset::east_opt(0).unwrap());
    off.timestamp_opt(ts, 0)
        .single()
        .map(|d| d.date_naive())
        .unwrap_or_default()
}

/// The night a sleep sample belongs to: the local date it **ends** on, where a
/// "day" runs noon→noon (a sample ending at 07:00 on the 26th or 23:00 on the
/// 25th both count toward the night of the 26th). See INVARIANTS.md.
pub fn sleep_night(end_ts: i64, offset_min: i32) -> NaiveDate {
    local_date(end_ts + 12 * 3600, offset_min)
}

pub fn fmt_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

/// Start-of-bucket label for a date.
pub fn bucket_label(d: NaiveDate, bucket: Bucket) -> String {
    match bucket {
        Bucket::Day => fmt_date(d),
        Bucket::Week => fmt_date(d - Duration::days(d.weekday().num_days_from_monday() as i64)),
        Bucket::Month => format!("{:04}-{:02}-01", d.year(), d.month()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Day,
    /// ISO weeks, labelled by their Monday.
    Week,
    Month,
}

impl Bucket {
    pub fn parse(s: &str) -> Option<Bucket> {
        match s {
            "day" => Some(Bucket::Day),
            "week" => Some(Bucket::Week),
            "month" => Some(Bucket::Month),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_export_dates_with_offset() {
        let (ts, off) = parse_hk_date("2022-02-27 19:45:35 -0700").unwrap();
        assert_eq!(ts, 1_646_016_335);
        assert_eq!(off, -420);
        assert_eq!(fmt_date(local_date(ts, off)), "2022-02-27");
        // The same instant is already the 28th in UTC.
        assert_eq!(fmt_date(local_date(ts, 0)), "2022-02-28");
        assert!(parse_hk_date("garbage").is_none());
    }

    #[test]
    fn sleep_night_is_noon_to_noon_by_end() {
        let (wake, off) = parse_hk_date("2021-10-26 07:50:09 -0700").unwrap();
        assert_eq!(fmt_date(sleep_night(wake, off)), "2021-10-26");
        let (late, off) = parse_hk_date("2021-10-25 23:59:00 -0700").unwrap();
        assert_eq!(fmt_date(sleep_night(late, off)), "2021-10-26");
        let (nap, off) = parse_hk_date("2021-10-26 11:59:00 -0700").unwrap();
        assert_eq!(fmt_date(sleep_night(nap, off)), "2021-10-26");
    }

    #[test]
    fn bucket_labels() {
        let d = parse_date("2026-10-15").unwrap(); // a Thursday
        assert_eq!(bucket_label(d, Bucket::Day), "2026-10-15");
        assert_eq!(bucket_label(d, Bucket::Week), "2026-10-12");
        assert_eq!(bucket_label(d, Bucket::Month), "2026-10-01");
    }
}
