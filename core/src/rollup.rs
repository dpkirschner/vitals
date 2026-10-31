//! Daily rollups: one row per (local date, metric) in `daily`, derived from
//! `records` and `workouts`. Aggregation happens here in Rust, not SQL, because
//! the rules below are not GROUP BY-shaped (INVARIANTS #2, #3):
//!
//! - **Additive metrics** (steps, distance, energy, …): an iPhone and a Watch
//!   both record the same walk. Sum per source per day, then keep the
//!   **largest single source**; never add sources together.
//! - **Levels** (resting HR, HRV, …): mean of all samples that day.
//! - **Sleep**: samples chain into blocks; a block belongs to the night its
//!   midpoint falls in, noon→noon. The longest block is the night and the
//!   rest are naps. Overlapping intervals from different sources are
//!   unioned, never summed.
//!
//! A sample belongs to the local day of its own start (at its own offset).

use chrono::{Duration, NaiveDate, NaiveTime};
use rusqlite::params;
use std::collections::{BTreeMap, HashMap};

use crate::time::{fmt_date, local_date, sleep_night};
use crate::units::canonical;
use crate::Store;

/// (record type, metric name) — additive, one-source rule.
const ADDITIVE: &[(&str, &str)] = &[
    ("HKQuantityTypeIdentifierStepCount", "steps"),
    (
        "HKQuantityTypeIdentifierDistanceWalkingRunning",
        "distance_m",
    ),
    ("HKQuantityTypeIdentifierActiveEnergyBurned", "active_kcal"),
    ("HKQuantityTypeIdentifierBasalEnergyBurned", "basal_kcal"),
    ("HKQuantityTypeIdentifierFlightsClimbed", "flights"),
    ("HKQuantityTypeIdentifierAppleExerciseTime", "exercise_min"),
];

/// (record type, metric name) — daily mean.
const LEVELS: &[(&str, &str)] = &[
    ("HKQuantityTypeIdentifierRestingHeartRate", "resting_hr"),
    (
        "HKQuantityTypeIdentifierHeartRateVariabilitySDNN",
        "hrv_sdnn_avg",
    ),
    (
        "HKQuantityTypeIdentifierWalkingHeartRateAverage",
        "walking_hr_avg",
    ),
    (
        "HKQuantityTypeIdentifierRespiratoryRate",
        "respiratory_rate_avg",
    ),
    ("HKQuantityTypeIdentifierVO2Max", "vo2max"),
];

const SLEEP: &str = "HKCategoryTypeIdentifierSleepAnalysis";

/// Every metric the `daily` table can hold, for docs and `?metrics=` validation.
pub const METRICS: &[&str] = &[
    "steps",
    "distance_m",
    "active_kcal",
    "basal_kcal",
    "flights",
    "exercise_min",
    "resting_hr",
    "hrv_sdnn_avg",
    "walking_hr_avg",
    "respiratory_rate_avg",
    "vo2max",
    "sleep_asleep_min",
    "sleep_in_bed_min",
    "sleep_awake_min",
    "sleep_deep_min",
    "sleep_rem_min",
    "sleep_core_min",
    "sleep_nap_min",
    "sleep_start",
    "sleep_end",
    "workouts",
    "workout_min",
];

struct Row {
    start: i64,
    end: i64,
    off: i32,
    num: Option<f64>,
    text: Option<String>,
    unit: Option<String>,
    source: i64,
}

fn rows(store: &Store, identifier: &str, lo: i64, hi: i64) -> Result<Vec<Row>, String> {
    let Some(type_id) = store
        .lookup_type_id(identifier)
        .map_err(|e| e.to_string())?
    else {
        return Ok(Vec::new());
    };
    let mut st = store
        .conn
        .prepare_cached(
            "SELECT start_ts, end_ts, tz_offset_min, value_num, value_text, unit, source_id
             FROM records WHERE type_id = ?1 AND start_ts >= ?2 AND start_ts < ?3",
        )
        .map_err(|e| e.to_string())?;
    let out = st
        .query_map(params![type_id, lo, hi], |r| {
            Ok(Row {
                start: r.get(0)?,
                end: r.get(1)?,
                off: r.get(2)?,
                num: r.get(3)?,
                text: r.get(4)?,
                unit: r.get(5)?,
                source: r.get(6)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Total minutes covered by a set of possibly-overlapping intervals.
pub fn union_minutes(mut iv: Vec<(i64, i64)>) -> f64 {
    iv.sort_unstable();
    let mut total = 0i64;
    let mut cur: Option<(i64, i64)> = None;
    for (s, e) in iv {
        match cur {
            Some((cs, ce)) if s <= ce => cur = Some((cs, ce.max(e))),
            Some((cs, ce)) => {
                total += ce - cs;
                cur = Some((s, e));
            }
            None => cur = Some((s, e)),
        }
    }
    if let Some((cs, ce)) = cur {
        total += ce - cs;
    }
    total as f64 / 60.0
}

type Days = BTreeMap<NaiveDate, BTreeMap<&'static str, f64>>;

fn additive(store: &Store, lo: i64, hi: i64, out: &mut Days) -> Result<(), String> {
    for (ty, metric) in ADDITIVE {
        let mut per_source: HashMap<(NaiveDate, i64), f64> = HashMap::new();
        for r in rows(store, ty, lo, hi)? {
            let Some(v) = r.num else { continue };
            let v = match r.unit.as_deref() {
                Some(u) => canonical(v, u).0,
                None => v,
            };
            *per_source
                .entry((local_date(r.start, r.off), r.source))
                .or_default() += v;
        }
        for ((day, _), v) in per_source {
            let slot = out.entry(day).or_default().entry(metric).or_insert(0.0);
            *slot = slot.max(v);
        }
    }
    Ok(())
}

fn levels(store: &Store, lo: i64, hi: i64, out: &mut Days) -> Result<(), String> {
    for (ty, metric) in LEVELS {
        let mut acc: HashMap<NaiveDate, (f64, usize)> = HashMap::new();
        for r in rows(store, ty, lo, hi)? {
            let Some(v) = r.num else { continue };
            let e = acc.entry(local_date(r.start, r.off)).or_default();
            e.0 += v;
            e.1 += 1;
        }
        for (day, (sum, n)) in acc {
            out.entry(day).or_default().insert(metric, sum / n as f64);
        }
    }
    Ok(())
}

/// Two sleep samples further apart than this start a new block.
const SLEEP_GAP_S: i64 = 2 * 3600;

#[derive(Default)]
struct Block {
    asleep: Vec<(i64, i64)>,
    in_bed: Vec<(i64, i64)>,
    awake: Vec<(i64, i64)>,
    deep: Vec<(i64, i64)>,
    rem: Vec<(i64, i64)>,
    core: Vec<(i64, i64)>,
    /// Every interval regardless of stage: how long the block lasted.
    all: Vec<(i64, i64)>,
}

impl Block {
    fn push(&mut self, stage: &str, iv: (i64, i64)) {
        self.all.push(iv);
        match stage {
            "InBed" => self.in_bed.push(iv),
            "Awake" => self.awake.push(iv),
            "AsleepDeep" => {
                self.deep.push(iv);
                self.asleep.push(iv);
            }
            "AsleepREM" => {
                self.rem.push(iv);
                self.asleep.push(iv);
            }
            "AsleepCore" => {
                self.core.push(iv);
                self.asleep.push(iv);
            }
            // Legacy "Asleep" and post-iOS16 "AsleepUnspecified".
            "Asleep" | "AsleepUnspecified" => self.asleep.push(iv),
            _ => {}
        }
    }
}

/// Sleep comes in blocks: samples of any stage and source chained by gaps of
/// at most `SLEEP_GAP_S`. Each block lands on the night of its midpoint
/// (`time::sleep_night`). The longest block that night is the night; the
/// asleep time of any other block is a nap (`sleep_nap_min`).
fn sleep(store: &Store, lo: i64, hi: i64, out: &mut Days) -> Result<(), String> {
    let mut samples: Vec<(i64, i64, i32, String)> = Vec::new();
    for r in rows(store, SLEEP, lo, hi)? {
        let Some(v) = r.text else { continue };
        let stage = v
            .strip_prefix("HKCategoryValueSleepAnalysis")
            .unwrap_or(&v)
            .to_string();
        samples.push((r.start, r.end, r.off, stage));
    }
    samples.sort_unstable_by_key(|s| (s.0, s.1));

    let mut blocks: Vec<(Block, i64, i64, i32)> = Vec::new();
    for (start, end, off, stage) in samples {
        match blocks.last_mut() {
            Some((n, _, b_end, b_off)) if start <= *b_end + SLEEP_GAP_S => {
                if end > *b_end {
                    *b_end = end;
                    *b_off = off;
                }
                n.push(&stage, (start, end));
            }
            _ => {
                let mut n = Block::default();
                n.push(&stage, (start, end));
                blocks.push((n, start, end, off));
            }
        }
    }

    let mut nights: HashMap<NaiveDate, Vec<Block>> = HashMap::new();
    for (n, start, end, off) in blocks {
        nights
            .entry(sleep_night(start, end, off))
            .or_default()
            .push(n);
    }
    for (night, mut blocks) in nights {
        let longest = blocks
            .iter()
            .map(|b| union_minutes(b.all.clone()))
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
            .unwrap_or_default();
        let n = blocks.swap_remove(longest);
        let day = out.entry(night).or_default();
        let nap: f64 = blocks.into_iter().map(|b| union_minutes(b.asleep)).sum();
        if nap > 0.0 {
            day.insert("sleep_nap_min", nap);
        }
        if !n.asleep.is_empty() {
            let start = n.asleep.iter().map(|i| i.0).min().unwrap_or_default();
            let end = n.asleep.iter().map(|i| i.1).max().unwrap_or_default();
            day.insert("sleep_start", start as f64);
            day.insert("sleep_end", end as f64);
            day.insert("sleep_asleep_min", union_minutes(n.asleep));
        }
        for (metric, iv) in [
            ("sleep_in_bed_min", n.in_bed),
            ("sleep_awake_min", n.awake),
            ("sleep_deep_min", n.deep),
            ("sleep_rem_min", n.rem),
            ("sleep_core_min", n.core),
        ] {
            if !iv.is_empty() {
                day.insert(metric, union_minutes(iv));
            }
        }
    }
    Ok(())
}

fn workouts(store: &Store, lo: i64, hi: i64, out: &mut Days) -> Result<(), String> {
    let mut st = store
        .conn
        .prepare_cached(
            "SELECT start_ts, end_ts, tz_offset_min, duration_s FROM workouts
             WHERE start_ts >= ?1 AND start_ts < ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![lo, hi], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, Option<f64>>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (start, end, off, dur) = row.map_err(|e| e.to_string())?;
        let day = out.entry(local_date(start, off)).or_default();
        *day.entry("workouts").or_insert(0.0) += 1.0;
        *day.entry("workout_min").or_insert(0.0) += dur.unwrap_or((end - start) as f64) / 60.0;
    }
    Ok(())
}

/// Recompute `daily` for every local date in `from..=to`. Returns the number
/// of dates that ended up with at least one metric.
pub fn rebuild(store: &Store, from: NaiveDate, to: NaiveDate) -> Result<usize, String> {
    // Offsets span −12h…+14h, and sleep reaches back ~a day before its night,
    // so read a 2-day margin either side of the UTC midnights.
    let midnight = |d: NaiveDate| d.and_time(NaiveTime::MIN).and_utc().timestamp();
    let lo = midnight(from - Duration::days(2));
    let hi = midnight(to + Duration::days(3));

    let mut days = Days::new();
    additive(store, lo, hi, &mut days)?;
    levels(store, lo, hi, &mut days)?;
    sleep(store, lo, hi, &mut days)?;
    workouts(store, lo, hi, &mut days)?;

    let conn = &store.conn;
    conn.execute(
        "DELETE FROM daily WHERE date >= ?1 AND date <= ?2",
        params![fmt_date(from), fmt_date(to)],
    )
    .map_err(|e| e.to_string())?;
    let mut ins = conn
        .prepare_cached("INSERT INTO daily(date, metric, value) VALUES (?1, ?2, ?3)")
        .map_err(|e| e.to_string())?;
    let mut written = 0;
    for (day, metrics) in days.range(from..=to) {
        if metrics.is_empty() {
            continue;
        }
        let d = fmt_date(*day);
        for (m, v) in metrics {
            ins.execute(params![d, m, v]).map_err(|e| e.to_string())?;
        }
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::import_path;

    fn imported() -> Store {
        import_xml(include_str!("../../examples/export-sample.xml"))
    }

    fn import_xml(xml_text: &str) -> Store {
        let dir = std::env::temp_dir().join(format!(
            "vitals-rollup-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "_")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let xml = dir.join("export.xml");
        std::fs::write(&xml, xml_text).unwrap();
        let s = Store::open_in_memory().unwrap();
        import_path(&s, xml.to_str().unwrap(), |_| {}).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        s
    }

    fn metric(s: &Store, date: &str, m: &str) -> Option<f64> {
        s.conn
            .query_row(
                "SELECT value FROM daily WHERE date = ?1 AND metric = ?2",
                params![date, m],
                |r| r.get(0),
            )
            .ok()
    }

    #[test]
    fn additive_metrics_keep_the_largest_single_source() {
        let s = imported();
        // iPhone 1000 + 2000 = 3000; Watch 2500 + 1000 = 3500. Not 6500.
        assert_eq!(metric(&s, "2026-03-01", "steps"), Some(3500.0));
        assert_eq!(metric(&s, "2026-03-01", "distance_m"), Some(1500.0));
        assert_eq!(metric(&s, "2026-03-01", "active_kcal"), Some(120.5));
    }

    #[test]
    fn samples_bucket_by_their_own_offset() {
        let s = imported();
        // 00:30 −0500 on the 4th is 21:30 −0800 on the 3rd; it counts on the 4th.
        assert_eq!(metric(&s, "2026-03-03", "steps"), Some(400.0));
        assert_eq!(metric(&s, "2026-03-04", "steps"), Some(600.0));
    }

    #[test]
    fn levels_average() {
        let s = imported();
        assert_eq!(metric(&s, "2026-03-01", "resting_hr"), Some(52.0));
        assert_eq!(metric(&s, "2026-03-02", "resting_hr"), Some(56.0));
        assert_eq!(metric(&s, "2026-03-01", "hrv_sdnn_avg"), Some(42.5));
    }

    #[test]
    fn sleep_unions_sources_and_lands_on_the_wake_date() {
        let s = imported();
        // Watch asleep 23:10–02:30 + 02:40–06:40; iPhone asleep 23:30–06:30
        // fills the gap → one span 23:10–06:40 = 450 min.
        assert_eq!(metric(&s, "2026-03-02", "sleep_asleep_min"), Some(450.0));
        assert_eq!(metric(&s, "2026-03-02", "sleep_in_bed_min"), Some(480.0));
        assert_eq!(metric(&s, "2026-03-02", "sleep_deep_min"), Some(60.0));
        assert_eq!(metric(&s, "2026-03-02", "sleep_rem_min"), Some(30.0));
        assert_eq!(metric(&s, "2026-03-02", "sleep_core_min"), Some(350.0));
        assert_eq!(metric(&s, "2026-03-02", "sleep_awake_min"), Some(10.0));
        assert_eq!(metric(&s, "2026-03-01", "sleep_asleep_min"), None);
        let start = metric(&s, "2026-03-02", "sleep_start").unwrap() as i64;
        let (expect, _) = crate::time::parse_hk_date("2026-03-01 23:10:00 -0800").unwrap();
        assert_eq!(start, expect);
    }

    /// Synthetic: a nap, the next night, and a late night that ends after noon.
    fn naps_and_late_nights() -> Store {
        let rec = |src: &str, start: &str, end: &str, stage: &str| {
            format!(
                r#" <Record type="HKCategoryTypeIdentifierSleepAnalysis" sourceName="{src}" sourceVersion="1" creationDate="{end}" startDate="{start}" endDate="{end}" value="HKCategoryValueSleepAnalysis{stage}"/>"#
            )
        };
        let recs = [
            rec(
                "Test Watch",
                "2026-04-01 14:00:00 -0700",
                "2026-04-01 14:45:00 -0700",
                "AsleepUnspecified",
            ),
            rec(
                "Test iPhone",
                "2026-04-01 22:50:00 -0700",
                "2026-04-02 07:10:00 -0700",
                "InBed",
            ),
            rec(
                "Test Watch",
                "2026-04-01 23:00:00 -0700",
                "2026-04-02 03:00:00 -0700",
                "AsleepCore",
            ),
            rec(
                "Test Watch",
                "2026-04-02 03:00:00 -0700",
                "2026-04-02 03:20:00 -0700",
                "Awake",
            ),
            rec(
                "Test Watch",
                "2026-04-02 03:20:00 -0700",
                "2026-04-02 07:00:00 -0700",
                "AsleepCore",
            ),
            rec(
                "Test Watch",
                "2026-04-03 03:30:00 -0700",
                "2026-04-03 12:40:00 -0700",
                "AsleepUnspecified",
            ),
        ];
        import_xml(&format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<HealthData locale="en_US">
 <ExportDate value="2026-04-04 09:00:00 -0700"/>
{}
</HealthData>
"#,
            recs.join("\n")
        ))
    }

    #[test]
    fn a_nap_is_not_the_next_nights_bedtime() {
        let s = naps_and_late_nights();
        let at = |t: &str| crate::time::parse_hk_date(t).unwrap().0 as f64;
        let start = metric(&s, "2026-04-02", "sleep_start");
        assert_eq!(start, Some(at("2026-04-01 23:00:00 -0700")));
        assert_eq!(metric(&s, "2026-04-02", "sleep_asleep_min"), Some(460.0));
        assert_eq!(metric(&s, "2026-04-02", "sleep_in_bed_min"), Some(500.0));
        assert_eq!(metric(&s, "2026-04-02", "sleep_nap_min"), Some(45.0));
        assert_eq!(metric(&s, "2026-04-01", "sleep_asleep_min"), None);
    }

    #[test]
    fn a_late_night_ending_after_noon_keeps_its_date() {
        let s = naps_and_late_nights();
        let at = |t: &str| crate::time::parse_hk_date(t).unwrap().0 as f64;
        assert_eq!(metric(&s, "2026-04-03", "sleep_asleep_min"), Some(550.0));
        let end = metric(&s, "2026-04-03", "sleep_end");
        assert_eq!(end, Some(at("2026-04-03 12:40:00 -0700")));
        assert_eq!(metric(&s, "2026-04-03", "sleep_nap_min"), None);
        assert_eq!(metric(&s, "2026-04-04", "sleep_asleep_min"), None);
    }

    #[test]
    fn workouts_count_per_day() {
        let s = imported();
        assert_eq!(metric(&s, "2026-03-03", "workouts"), Some(1.0));
        assert_eq!(metric(&s, "2026-03-03", "workout_min"), Some(30.0));
        assert_eq!(metric(&s, "2026-03-04", "workout_min"), Some(20.0));
    }

    #[test]
    fn rebuild_is_repeatable() {
        let s = imported();
        let from = crate::time::parse_date("2026-02-28").unwrap();
        let to = crate::time::parse_date("2026-03-05").unwrap();
        let a = rebuild(&s, from, to).unwrap();
        let b = rebuild(&s, from, to).unwrap();
        assert_eq!(a, b);
        assert_eq!(metric(&s, "2026-03-01", "steps"), Some(3500.0));
    }

    #[test]
    fn union_minutes_merges_overlaps() {
        assert_eq!(
            union_minutes(vec![(0, 600), (300, 900), (1200, 1260)]),
            16.0
        );
        assert_eq!(union_minutes(vec![]), 0.0);
    }
}
