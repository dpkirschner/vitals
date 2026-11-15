//! One chart, picked by a strict set of query parameters: the data behind
//! the embeddable chart view (`/embed/chart`) that agent-avatar frames, and
//! `GET /api/chart` for anyone else who wants the same numbers. Modeled on
//! outflow's `core::chart`, with the same range and highlight spellings.
//!
//! The parameters are an allow-list (docs/API.md "Embed views"): an unknown
//! name, a repeated name or a bad value is a [`ParamError`] naming the
//! parameter, never a guess. agent-avatar validates against the same list
//! before it builds the URL.
//!
//! - `metric` (required): `weight`, `sleep`, `resting_hr`, `hrv`, `steps`,
//!   `training`, `vo2max`.
//! - `range`: `7d` `30d` `3m` (default) `6m` `12m` `ytd` `all`; or `from` /
//!   `to` as `YYYY-MM` or `YYYY-MM-DD` (a month `from` starts on its 1st, a
//!   month `to` ends on its last day; `to` past today is today). Not both.
//! - `highlight`: a month `2026-10`, a day `2026-10-03`, or `A..B` with
//!   either spelling at each end. It must overlap the range.
//!
//! Reads go through `core::query` (INVARIANTS #8): `daily` for the rollup
//! metrics, `series` for body mass.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use chrono::{Datelike, Days, Months, NaiveDate, TimeZone, Utc};
use serde::Serialize;

use crate::query::{self, DateRange, RecordQuery, SeriesQuery};
use crate::time::fmt_date;
use crate::Store;

/// What the chart plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartMetric {
    Weight,
    Sleep,
    RestingHr,
    Hrv,
    Steps,
    Training,
    Vo2max,
}

/// How a bucket's days combine, and what the headline says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A level read at a moment (weight, VO2 max): the headline is the
    /// latest value and its change over the span.
    Latest,
    /// A per-day value (sleep, steps, heart rate): buckets and the headline
    /// average the days that have one.
    Average,
    /// Minutes that add up (training): buckets sum their days, a day with
    /// none counts as 0, and the headline is the average complete bucket or
    /// the highlighted total.
    Total,
}

impl ChartMetric {
    pub const NAMES: &'static str = "weight, sleep, resting_hr, hrv, steps, training, vo2max";

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "weight" => Self::Weight,
            "sleep" => Self::Sleep,
            "resting_hr" => Self::RestingHr,
            "hrv" => Self::Hrv,
            "steps" => Self::Steps,
            "training" => Self::Training,
            "vo2max" => Self::Vo2max,
            _ => return None,
        })
    }

    fn title(self) -> &'static str {
        match self {
            Self::Weight => "Weight",
            Self::Sleep => "Sleep",
            Self::RestingHr => "Resting heart rate",
            Self::Hrv => "Heart rate variability",
            Self::Steps => "Steps",
            Self::Training => "Training",
            Self::Vo2max => "VO2 max",
        }
    }

    fn kind(self) -> Kind {
        match self {
            Self::Weight | Self::Vo2max => Kind::Latest,
            Self::Sleep | Self::RestingHr | Self::Hrv | Self::Steps => Kind::Average,
            Self::Training => Kind::Total,
        }
    }

    fn mark(self) -> ChartMark {
        match self {
            Self::Sleep | Self::Steps | Self::Training => ChartMark::Bar,
            _ => ChartMark::Line,
        }
    }

    /// The `/daily` metric behind it (`None`: weight reads `/series`), and
    /// the factor from the rollup's unit to the chart's.
    fn daily(self) -> Option<(&'static str, f64)> {
        Some(match self {
            Self::Weight => return None,
            Self::Sleep => ("sleep_asleep_min", 1.0 / 60.0),
            Self::RestingHr => ("resting_hr", 1.0),
            Self::Hrv => ("hrv_sdnn_avg", 1.0),
            Self::Steps => ("steps", 1.0),
            Self::Training => ("workout_min", 1.0),
            Self::Vo2max => ("vo2max", 1.0),
        })
    }

    fn unit(self) -> &'static str {
        match self {
            Self::Weight => "kg",
            Self::Sleep => "h",
            Self::RestingHr => "bpm",
            Self::Hrv => "ms",
            Self::Steps => "steps",
            Self::Training => "min",
            Self::Vo2max => "mL/kg·min",
        }
    }

    /// Today's value is still growing.
    fn cumulative(self) -> bool {
        matches!(self, Self::Steps | Self::Training)
    }
}

/// One point per day, week (Monday) or month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartStep {
    Day,
    Week,
    Month,
}

/// Bars or a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartMark {
    Bar,
    Line,
}

/// A parameter the chart can't use, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamError {
    pub param: String,
    pub reason: String,
}

impl ParamError {
    fn new(param: &str, reason: impl Into<String>) -> Self {
        ParamError {
            param: param.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.param, self.reason)
    }
}

impl std::error::Error for ParamError {}

/// The highlighted days, inclusive at both ends, and how they were spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    from: NaiveDate,
    to: NaiveDate,
    from_month: bool,
    to_month: bool,
}

/// Validated parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartRequest {
    pub metric: ChartMetric,
    /// First day; `None` = from the first day with data.
    pub since: Option<NaiveDate>,
    /// Last day (inclusive), never after today.
    pub until: NaiveDate,
    pub range_label: String,
    highlight: Option<Span>,
}

pub const PARAMS: [&str; 5] = ["metric", "range", "from", "to", "highlight"];

/// Checks the query pairs against the allow-list. `today` anchors the
/// presets and caps `to`.
pub fn parse_params(
    pairs: &[(String, String)],
    today: NaiveDate,
) -> Result<ChartRequest, ParamError> {
    let mut got: HashMap<&str, &str> = HashMap::new();
    for (k, v) in pairs {
        let Some(name) = PARAMS.iter().find(|p| **p == k.as_str()) else {
            return Err(ParamError::new(
                k,
                format!("not a chart parameter (use {})", PARAMS.join(", ")),
            ));
        };
        if got.insert(name, v.trim()).is_some() {
            return Err(ParamError::new(name, "given more than once"));
        }
    }

    let metric = match got.get("metric") {
        None | Some(&"") => return Err(ParamError::new("metric", "required")),
        Some(m) => ChartMetric::parse(m).ok_or_else(|| {
            ParamError::new("metric", format!("use one of {}", ChartMetric::NAMES))
        })?,
    };

    let days = |n: u64, label: &str| (Some(today - Days::new(n - 1)), today, label.to_string());
    let months = |n: u32, label: &str| {
        let since = first_of_month(today) - Months::new(n - 1);
        (Some(since), today, label.to_string())
    };
    let (since, until, range_label) = match (got.get("range"), got.get("from"), got.get("to")) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(ParamError::new("range", "give range or from/to, not both"))
        }
        (Some(r), None, None) => match *r {
            "7d" => days(7, "Last 7 days"),
            "30d" => days(30, "Last 30 days"),
            "3m" => months(3, "Last 3 months"),
            "6m" => months(6, "Last 6 months"),
            "12m" => months(12, "Last 12 months"),
            "ytd" => (
                Some(NaiveDate::from_ymd_opt(today.year(), 1, 1).expect("Jan 1")),
                today,
                "Year to date".into(),
            ),
            "all" => (None, today, "All history".into()),
            _ => {
                return Err(ParamError::new(
                    "range",
                    "use one of 7d, 30d, 3m, 6m, 12m, ytd, all",
                ))
            }
        },
        (None, None, None) => months(3, "Last 3 months"),
        (None, from, to) => {
            let from = from.map(|s| parse_end("from", s, false)).transpose()?;
            let to = to.map(|s| parse_end("to", s, true)).transpose()?;
            let until = to.map_or(today, |(d, _)| d.min(today));
            if let Some((f, _)) = from {
                if f > until {
                    return Err(ParamError::new("from", "after to (or after today)"));
                }
            }
            let label = match (from, to) {
                (Some(f), Some(t)) => format!("{} – {}", label_of(f), label_of(t)),
                (Some(f), None) => format!("Since {}", label_of(f)),
                (None, Some(t)) => format!("Through {}", label_of(t)),
                (None, None) => unreachable!("matched above"),
            };
            (from.map(|(d, _)| d), until, label)
        }
    };

    let highlight = match got.get("highlight") {
        None | Some(&"") => None,
        Some(h) => {
            let (a, b) = h.split_once("..").unwrap_or((h, h));
            let (from, from_month) = parse_end("highlight", a, false)?;
            let (to, to_month) = parse_end("highlight", b, true)?;
            if from > to {
                return Err(ParamError::new("highlight", "start is after end"));
            }
            if to < since.unwrap_or(NaiveDate::MIN) || from > until {
                return Err(ParamError::new("highlight", "outside the range"));
            }
            Some(Span {
                from,
                to,
                from_month,
                to_month,
            })
        }
    };

    Ok(ChartRequest {
        metric,
        since,
        until,
        range_label,
        highlight,
    })
}

/// `YYYY-MM` or `YYYY-MM-DD`. A month is its first day, or with `end` its
/// last day. Returns the day and whether it was spelled as a month.
fn parse_end(param: &str, s: &str, end: bool) -> Result<(NaiveDate, bool), ParamError> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok((d, false));
    }
    let bad = || ParamError::new(param, format!("bad date {s:?}: use YYYY-MM or YYYY-MM-DD"));
    if s.len() != 7 {
        return Err(bad());
    }
    let first = NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d").map_err(|_| bad())?;
    Ok((if end { last_of_month(first) } else { first }, true))
}

fn first_of_month(d: NaiveDate) -> NaiveDate {
    d.with_day(1).expect("day 1 exists")
}

fn last_of_month(d: NaiveDate) -> NaiveDate {
    first_of_month(d) + Months::new(1) - Days::new(1)
}

fn label_of((d, month): (NaiveDate, bool)) -> String {
    if month {
        d.format("%b %Y").to_string()
    } else {
        d.format("%b %-d, %Y").to_string()
    }
}

fn short_day(d: NaiveDate) -> String {
    d.format("%b %-d").to_string()
}

/// One bar or one point on the line, keyed by its bucket's first day.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartPoint {
    /// `YYYY-MM-DD`: the day, the week's Monday or the month's 1st.
    pub key: String,
    /// In the chart's `unit`. `null` on a bar with no data (lines skip
    /// such buckets).
    pub value: Option<f64>,
    pub highlighted: bool,
}

/// The one big figure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartHeadline {
    pub label: String,
    /// `null` when the span has no data.
    pub value: Option<f64>,
    /// Weight and VO2 max: the change since `change_since`.
    pub change: Option<f64>,
    /// `YYYY-MM-DD` of the first value in the span, when `change` is set.
    pub change_since: Option<String>,
}

/// `GET /api/chart`: what the embed view draws.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartView {
    pub metric: ChartMetric,
    pub step: ChartStep,
    pub mark: ChartMark,
    pub title: String,
    /// `kg` or `lb` (weight, following the latest record), `h`, `bpm`,
    /// `ms`, `steps`, `min`, `mL/kg·min`.
    pub unit: String,
    /// What a bar or point is: "Hours a night", "Minutes a week", …
    pub per: String,
    pub range_label: String,
    /// Oldest first.
    pub points: Vec<ChartPoint>,
    /// The key of the last bucket when today is in it and its value is
    /// still growing (steps, training).
    pub partial_key: Option<String>,
    pub headline: ChartHeadline,
}

/// Builds the chart. `today` decides which bucket is still in progress.
pub fn chart(s: &Store, req: &ChartRequest, today: NaiveDate) -> Result<ChartView, String> {
    let (days, unit) = match req.metric.daily() {
        Some((name, scale)) => {
            let range = DateRange {
                since: req.since.map(fmt_date),
                until: Some(fmt_date(req.until)),
            };
            let days = query::daily(s, &range, &[name.to_string()])?
                .into_iter()
                .filter_map(|r| {
                    let d = crate::time::parse_date(&r.date)?;
                    Some((d, r.metrics.get(name)? * scale))
                })
                .collect();
            (days, req.metric.unit())
        }
        None => weight(s, req)?,
    };
    Ok(build(req, &days, unit, today))
}

/// Daily average body mass, in the unit of the latest record (`lb` stays
/// `lb`; anything else is shown in `kg`). No body mass type = no data.
fn weight(
    s: &Store,
    req: &ChartRequest,
) -> Result<(BTreeMap<NaiveDate, f64>, &'static str), String> {
    if let Err(e) = query::resolve_type(s, "BodyMass") {
        return if e.starts_with("db:") {
            Err(e)
        } else {
            Ok((BTreeMap::new(), "kg"))
        };
    }
    let latest = query::records(
        s,
        &RecordQuery {
            type_: Some("BodyMass".into()),
            limit: Some(1),
            ..Default::default()
        },
    )?;
    let lb = latest.items.first().and_then(|r| r.unit.as_deref()) == Some("lb");
    // Epoch bounds a day wider than the range on each side, since a
    // sample's local day follows its own offset; then trim by local day.
    let epoch = |d: NaiveDate| {
        Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("midnight"))
            .timestamp()
    };
    let series = query::series(
        s,
        &SeriesQuery {
            type_: "BodyMass".into(),
            since: req.since.map(|d| epoch(d - Days::new(1))),
            until: Some(epoch(req.until + Days::new(2))),
            bucket: Some("day".into()),
            agg: Some("avg".into()),
        },
    )?;
    let factor = if lb { 1.0 / 0.453_592_37 } else { 1.0 };
    let days = series
        .points
        .into_iter()
        .filter_map(|p| Some((crate::time::parse_date(&p.bucket)?, p.value * factor)))
        .filter(|(d, _)| req.since.is_none_or(|s| *d >= s) && *d <= req.until)
        .collect();
    Ok((days, if lb { "lb" } else { "kg" }))
}

fn bucket_start(d: NaiveDate, step: ChartStep) -> NaiveDate {
    match step {
        ChartStep::Day => d,
        ChartStep::Week => d - Days::new(d.weekday().num_days_from_monday() as u64),
        ChartStep::Month => first_of_month(d),
    }
}

fn bucket_end(start: NaiveDate, step: ChartStep) -> NaiveDate {
    match step {
        ChartStep::Day => start,
        ChartStep::Week => start + Days::new(6),
        ChartStep::Month => last_of_month(start),
    }
}

fn next_bucket(start: NaiveDate, step: ChartStep) -> NaiveDate {
    bucket_end(start, step) + Days::new(1)
}

fn mean(vals: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = vals.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

/// The chart from one value per local day. Pure, so tests can drive it
/// without a store.
fn build(
    req: &ChartRequest,
    days: &BTreeMap<NaiveDate, f64>,
    unit: &str,
    today: NaiveDate,
) -> ChartView {
    let metric = req.metric;
    let kind = metric.kind();
    let until = req.until;
    let first = req
        .since
        .unwrap_or_else(|| days.keys().next().copied().unwrap_or(until))
        .min(until);
    // Readable bar and point counts at any range: days up to ~3 months,
    // weeks up to 2 years, months beyond. Training never goes daily.
    let span = (until - first).num_days() + 1;
    let step = match span {
        _ if span > 731 => ChartStep::Month,
        _ if span > 92 || kind == Kind::Total => ChartStep::Week,
        _ => ChartStep::Day,
    };
    let hl = req.highlight;
    let lit = |a: NaiveDate, b: NaiveDate| hl.is_some_and(|h| a <= h.to && b >= h.from);

    let mut points = Vec::new();
    let mut start = bucket_start(first, step);
    while start <= until {
        let end = bucket_end(start, step);
        let vals = days
            .range(start.max(first)..=end.min(until))
            .map(|(_, v)| *v);
        let value = match kind {
            Kind::Total => Some(vals.sum()),
            _ => mean(vals),
        };
        if value.is_some() || metric.mark() == ChartMark::Bar {
            points.push(ChartPoint {
                key: fmt_date(start),
                value,
                highlighted: lit(start, end),
            });
        }
        start = next_bucket(start, step);
    }
    let last_start = bucket_start(until, step);
    let partial_key =
        (metric.cumulative() && until == today && !points.is_empty()).then(|| fmt_date(last_start));

    let span_label = |h: Span| {
        if h.from == h.to {
            short_day(h.from)
        } else if h.from_month && h.to_month && first_of_month(h.from) == first_of_month(h.to) {
            h.from.format("%b %Y").to_string()
        } else {
            format!(
                "{} – {}",
                label_of((h.from, h.from_month)),
                label_of((h.to, h.to_month))
            )
        }
    };
    // The days the headline reads: the highlight, or the whole range, less
    // today when its value is still growing.
    let (lo, hi) = match hl {
        Some(h) => (h.from.max(first), h.to.min(until)),
        None if metric.cumulative() && until == today => (first, until - Days::new(1)),
        None => (first, until),
    };
    let window: Vec<(NaiveDate, f64)> = if lo <= hi {
        days.range(lo..=hi).map(|(d, v)| (*d, *v)).collect()
    } else {
        Vec::new()
    };

    let headline = match kind {
        Kind::Latest => {
            let last = window.last().copied();
            let first_v = window.first().copied();
            let change = match (first_v, last) {
                (Some(a), Some(b)) if a.0 != b.0 => Some((b.1 - a.1, a.0)),
                _ => None,
            };
            ChartHeadline {
                label: match (hl, last) {
                    (Some(h), _) if h.from == h.to => short_day(h.from),
                    (_, Some((d, _))) => short_day(d),
                    (Some(h), None) => span_label(h),
                    (None, None) => "No readings".into(),
                },
                value: last.map(|(_, v)| v),
                change: change.map(|(c, _)| c),
                change_since: change.map(|(_, d)| fmt_date(d)),
            }
        }
        Kind::Average => ChartHeadline {
            label: match (hl, metric) {
                (Some(h), _) => span_label(h),
                (None, ChartMetric::Sleep) => "Average night".into(),
                (None, ChartMetric::Steps) => "Average day".into(),
                (None, _) => "Average".into(),
            },
            value: mean(window.iter().map(|(_, v)| *v)),
            change: None,
            change_since: None,
        },
        Kind::Total => match hl {
            Some(h) => ChartHeadline {
                label: span_label(h),
                value: Some(window.iter().map(|(_, v)| *v).sum()),
                change: None,
                change_since: None,
            },
            None => {
                // Buckets wholly inside the range and not still in progress.
                let whole: Vec<f64> = points
                    .iter()
                    .filter(|p| Some(&p.key) != partial_key.as_ref())
                    .filter(|p| {
                        let s = crate::time::parse_date(&p.key).expect("own key");
                        s >= first && bucket_end(s, step) <= until
                    })
                    .filter_map(|p| p.value)
                    .collect();
                let unit_word = match step {
                    ChartStep::Month => "month",
                    _ => "week",
                };
                match mean(whole.into_iter()) {
                    Some(v) => ChartHeadline {
                        label: format!("Average {unit_word}"),
                        value: Some(v),
                        change: None,
                        change_since: None,
                    },
                    None => ChartHeadline {
                        label: format!("So far this {unit_word}"),
                        value: points.last().and_then(|p| p.value),
                        change: None,
                        change_since: None,
                    },
                }
            }
        },
    };

    let per = match (kind, step) {
        (Kind::Total, ChartStep::Month) => "Minutes a month",
        (Kind::Total, _) => "Minutes a week",
        (_, ChartStep::Day) => match metric {
            ChartMetric::Sleep => "Hours asleep a night",
            ChartMetric::Steps => "Steps a day",
            _ => "Daily",
        },
        (_, ChartStep::Week) => "Weekly average",
        (_, ChartStep::Month) => "Monthly average",
    };

    ChartView {
        metric,
        step,
        mark: metric.mark(),
        title: metric.title().into(),
        unit: unit.into(),
        per: per.into(),
        range_label: req.range_label.clone(),
        points,
        partial_key,
        headline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_path;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn today() -> NaiveDate {
        // A Saturday.
        day(2026, 11, 14)
    }

    fn parse(q: &str) -> Result<ChartRequest, ParamError> {
        let pairs: Vec<(String, String)> = q
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        parse_params(&pairs, today())
    }

    fn err_param(q: &str) -> String {
        parse(q).unwrap_err().param
    }

    #[test]
    fn rejects_anything_off_the_list() {
        assert_eq!(err_param(""), "metric");
        assert_eq!(err_param("metric=mood"), "metric");
        assert_eq!(err_param("metric=weight&url=https://example.com"), "url");
        assert_eq!(err_param("metric=weight&metric=sleep"), "metric");
        assert_eq!(err_param("metric=weight&range=2w"), "range");
        assert_eq!(err_param("metric=weight&range=3m&from=2026-01"), "range");
        assert_eq!(err_param("metric=weight&from=2026-13"), "from");
        assert_eq!(err_param("metric=weight&from=2026-11-20"), "from");
        assert_eq!(err_param("metric=weight&highlight=yesterday"), "highlight");
        assert_eq!(
            err_param("metric=weight&range=7d&highlight=2026-01"),
            "highlight"
        );
        assert_eq!(
            err_param("metric=weight&highlight=2026-11-10..2026-11-01"),
            "highlight"
        );
        let e = parse("metric=weight&theme=dark").unwrap_err();
        assert_eq!(
            e.to_string(),
            "theme: not a chart parameter (use metric, range, from, to, highlight)"
        );
    }

    #[test]
    fn ranges_resolve_to_days() {
        let r = parse("metric=sleep").unwrap();
        assert_eq!((r.since, r.until), (Some(day(2026, 9, 1)), today()));
        assert_eq!(r.range_label, "Last 3 months");
        let r = parse("metric=sleep&range=7d").unwrap();
        assert_eq!(r.since, Some(day(2026, 11, 8)));
        let r = parse("metric=sleep&range=30d").unwrap();
        assert_eq!(r.since, Some(day(2026, 10, 16)));
        let r = parse("metric=sleep&range=ytd").unwrap();
        assert_eq!(r.since, Some(day(2026, 1, 1)));
        let r = parse("metric=sleep&range=all").unwrap();
        assert_eq!(r.since, None);
        let r = parse("metric=sleep&from=2026-02&to=2026-03").unwrap();
        assert_eq!(
            (r.since, r.until),
            (Some(day(2026, 2, 1)), day(2026, 3, 31))
        );
        assert_eq!(r.range_label, "Feb 2026 – Mar 2026");
        let r = parse("metric=sleep&to=2027-01-01").unwrap();
        assert_eq!((r.since, r.until), (None, today()));
    }

    fn req(q: &str) -> ChartRequest {
        parse(q).unwrap()
    }

    fn vals(v: &ChartView) -> Vec<(&str, Option<f64>, bool)> {
        v.points
            .iter()
            .map(|p| (p.key.as_str(), p.value, p.highlighted))
            .collect()
    }

    #[test]
    fn sleep_bars_per_night_with_gaps_and_an_average() {
        let days = BTreeMap::from([
            (day(2026, 11, 8), 7.0),
            (day(2026, 11, 9), 6.0),
            (day(2026, 11, 11), 8.0),
        ]);
        let v = build(&req("metric=sleep&range=7d"), &days, "h", today());
        assert_eq!((v.step, v.mark), (ChartStep::Day, ChartMark::Bar));
        assert_eq!(v.points.len(), 7);
        assert_eq!(
            v.points[2].value, None,
            "a night with no data is an empty bar"
        );
        assert_eq!(v.partial_key, None, "last night's sleep is complete");
        assert_eq!(v.headline.label, "Average night");
        assert_eq!(v.headline.value, Some(7.0));
        assert_eq!(v.per, "Hours asleep a night");
    }

    #[test]
    fn longer_ranges_go_weekly_then_monthly() {
        let mut days = BTreeMap::new();
        let mut d = day(2024, 1, 1);
        while d <= today() {
            days.insert(d, 60.0);
            d = d + Days::new(1);
        }
        let v = build(&req("metric=resting_hr&range=12m"), &days, "bpm", today());
        assert_eq!(v.step, ChartStep::Week);
        assert_eq!(
            v.points[0].key, "2025-12-01",
            "the Monday on or before Dec 1"
        );
        assert_eq!(v.per, "Weekly average");
        let v = build(&req("metric=resting_hr&range=all"), &days, "bpm", today());
        assert_eq!(v.step, ChartStep::Month);
        assert_eq!(v.points[0].key, "2024-01-01");
        assert_eq!(v.points.len(), 35);
    }

    #[test]
    fn a_line_skips_days_without_readings_and_reports_change() {
        let days = BTreeMap::from([
            (day(2026, 10, 20), 80.0),
            (day(2026, 11, 1), 79.0),
            (day(2026, 11, 12), 78.5),
        ]);
        let v = build(&req("metric=weight&range=30d"), &days, "lb", today());
        assert_eq!(v.mark, ChartMark::Line);
        assert_eq!(v.points.len(), 3);
        assert_eq!(v.unit, "lb");
        assert_eq!(v.headline.label, "Nov 12");
        assert_eq!(v.headline.value, Some(78.5));
        assert_eq!(v.headline.change, Some(-1.5));
        assert_eq!(v.headline.change_since.as_deref(), Some("2026-10-20"));

        let v = build(
            &req("metric=weight&range=30d&highlight=2026-11-01"),
            &days,
            "lb",
            today(),
        );
        assert_eq!(v.headline.label, "Nov 1");
        assert_eq!(v.headline.value, Some(79.0));
        assert_eq!(v.headline.change, None);
        assert_eq!(
            vals(&v)
                .iter()
                .filter(|p| p.2)
                .map(|p| p.0)
                .collect::<Vec<_>>(),
            ["2026-11-01"]
        );
    }

    #[test]
    fn training_sums_weeks_and_marks_this_one_in_progress() {
        let days = BTreeMap::from([
            (day(2026, 10, 27), 30.0),
            (day(2026, 10, 29), 45.0),
            (day(2026, 11, 3), 60.0),
            (day(2026, 11, 13), 20.0),
        ]);
        let v = build(&req("metric=training&range=30d"), &days, "min", today());
        assert_eq!(v.step, ChartStep::Week, "training is never daily");
        assert_eq!(
            vals(&v),
            [
                ("2026-10-12", Some(0.0), false),
                ("2026-10-19", Some(0.0), false),
                ("2026-10-26", Some(75.0), false),
                ("2026-11-02", Some(60.0), false),
                ("2026-11-09", Some(20.0), false),
            ]
        );
        assert_eq!(v.partial_key.as_deref(), Some("2026-11-09"));
        // Whole weeks only: Oct 19, Oct 26, Nov 2 (Oct 12 starts before the
        // range, Nov 9 is in progress).
        assert_eq!(v.headline.label, "Average week");
        assert_eq!(v.headline.value, Some(45.0));

        let v = build(
            &req("metric=training&range=30d&highlight=2026-10-26..2026-11-08"),
            &days,
            "min",
            today(),
        );
        assert_eq!(v.headline.label, "Oct 26, 2026 – Nov 8, 2026");
        assert_eq!(v.headline.value, Some(135.0));
        assert_eq!(v.points.iter().filter(|p| p.highlighted).count(), 2);
    }

    #[test]
    fn steps_leave_today_out_of_the_average() {
        let days = BTreeMap::from([(day(2026, 11, 13), 9000.0), (day(2026, 11, 14), 1200.0)]);
        let v = build(&req("metric=steps&range=7d"), &days, "steps", today());
        assert_eq!(v.partial_key.as_deref(), Some("2026-11-14"));
        assert_eq!(v.headline.value, Some(9000.0));
        let v = build(
            &req("metric=steps&from=2026-11-01&to=2026-11-13"),
            &days,
            "steps",
            today(),
        );
        assert_eq!(
            v.partial_key, None,
            "a range ending before today is complete"
        );
    }

    #[test]
    fn a_month_highlight_is_labelled_as_one() {
        let days = BTreeMap::from([(day(2026, 10, 3), 50.0), (day(2026, 10, 9), 54.0)]);
        let v = build(
            &req("metric=hrv&range=3m&highlight=2026-10"),
            &days,
            "ms",
            today(),
        );
        assert_eq!(v.headline.label, "Oct 2026");
        assert_eq!(v.headline.value, Some(52.0));
    }

    #[test]
    fn an_empty_archive_draws_nothing() {
        let v = build(
            &req("metric=vo2max&range=all"),
            &BTreeMap::new(),
            "mL/kg·min",
            today(),
        );
        assert!(v.points.is_empty());
        assert_eq!(v.headline.value, None);
        assert_eq!(v.headline.label, "No readings");
    }

    fn fixture() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "vitals-chart-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "_")
        ));
        std::fs::create_dir_all(dir.join("workout-routes")).unwrap();
        let xml = dir.join("export.xml");
        std::fs::write(&xml, include_str!("../../examples/export-sample.xml")).unwrap();
        std::fs::write(
            dir.join("workout-routes/route_sample.gpx"),
            include_str!("../../examples/route-sample.gpx"),
        )
        .unwrap();
        let s = Store::open_in_memory().unwrap();
        import_path(&s, xml.to_str().unwrap(), |_| {}).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        s
    }

    #[test]
    fn reads_the_store_through_query() {
        let s = fixture();
        let pairs = |q: &str| -> Vec<(String, String)> {
            q.split('&')
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap();
                    (k.into(), v.into())
                })
                .collect()
        };
        let at = day(2026, 3, 5);
        let r = parse_params(&pairs("metric=resting_hr&range=7d"), at).unwrap();
        let v = chart(&s, &r, at).unwrap();
        assert_eq!(v.headline.value, Some(54.0), "{v:?}");
        let r = parse_params(&pairs("metric=sleep&range=7d"), at).unwrap();
        let v = chart(&s, &r, at).unwrap();
        assert!(v.points.iter().any(|p| p.value.is_some()), "{v:?}");
        // The fixture has no body mass: an empty chart, not an error.
        let r = parse_params(&pairs("metric=weight&range=all"), at).unwrap();
        let v = chart(&s, &r, at).unwrap();
        assert!(v.points.is_empty());
    }
}
