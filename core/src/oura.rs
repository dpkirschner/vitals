//! Oura ring documents: the pure half of the Oura sync (INVARIANTS #7). The
//! `vitals-oura` crate does the OAuth and HTTP, then hands each response page
//! here as JSON text. This module validates it, stores each document as-is in
//! `oura_docs`, and turns the stored documents into `oura_*` daily metrics for
//! `rollup`.
//!
//! Oura data stays apart from `records`. A later Apple Health export carries
//! the records the Oura app wrote into Health, so merging the two would count
//! them twice. Oura's HRV is RMSSD (Apple's is SDNN), and its "light" stage is
//! not Apple's "core", so they are not the same metrics either.

use chrono::{Duration, NaiveDate};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

use crate::time::{fmt_date, parse_date};
use crate::Store;

/// The Oura collections vitals syncs, in fetch order. Each one is a path
/// under `/v2/usercollection/` and the `kind` column in `oura_docs`.
pub const KINDS: &[&str] = &[
    "daily_readiness",
    "daily_sleep",
    "sleep",
    "daily_activity",
    "daily_stress",
    "daily_spo2",
    "daily_cardiovascular_age",
    "vO2_max",
];

/// Days at the end of each sync that are fetched again and reconciled: Oura
/// revises recent documents, and removes or merges some.
pub const REFETCH_DAYS: i64 = 7;

/// One Oura document: its collection, Oura's id, its `day`, and the JSON
/// exactly as Oura sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct OuraDoc {
    pub kind: String,
    pub id: String,
    pub day: NaiveDate,
    pub json: String,
}

/// One page of a collection response.
#[derive(Debug)]
pub struct OuraPage {
    pub docs: Vec<OuraDoc>,
    pub next_token: Option<String>,
}

/// Parse one page of `GET /v2/usercollection/<kind>`. A document without a
/// string `id` or a `YYYY-MM-DD` `day` is an error, never skipped: a silent
/// skip would later look like a deletion.
pub fn parse_page(kind: &str, body: &str) -> Result<OuraPage, String> {
    if !KINDS.contains(&kind) {
        return Err(format!("unknown Oura collection {kind:?}"));
    }
    let v: Value = serde_json::from_str(body).map_err(|e| format!("oura {kind}: bad JSON: {e}"))?;
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("oura {kind}: response has no `data` array"))?;
    let mut docs = Vec::with_capacity(data.len());
    for (i, d) in data.iter().enumerate() {
        let id = d
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("oura {kind}: document {i} has no id"))?;
        let day = d
            .get("day")
            .and_then(Value::as_str)
            .and_then(parse_date)
            .ok_or_else(|| format!("oura {kind}: document {id} has no valid day"))?;
        docs.push(OuraDoc {
            kind: kind.to_string(),
            id: id.to_string(),
            day,
            json: d.to_string(),
        });
    }
    let next_token = match v.get("next_token") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(format!("oura {kind}: next_token is not a string")),
    };
    Ok(OuraPage { docs, next_token })
}

/// What one collection's fetch changed.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ApplyCounts {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

/// Store every document fetched for one collection over `fetched`
/// (inclusive days), in one transaction, then rebuild `daily` for the days
/// that changed.
///
/// Documents are upserted on (kind, id); an unchanged document writes nothing
/// (INVARIANTS #1). A stored document of this kind whose day is in `reconcile`
/// but which the fetch did not return is deleted: Oura removed or merged it.
/// Pass `reconcile` only over days this fetch fully covered.
pub fn apply(
    store: &Store,
    kind: &str,
    docs: &[OuraDoc],
    reconcile: Option<(NaiveDate, NaiveDate)>,
    fetched_ts: i64,
) -> Result<ApplyCounts, String> {
    let db = |e: rusqlite::Error| format!("db: {e}");
    let tx = store.conn.unchecked_transaction().map_err(db)?;
    let mut counts = ApplyCounts::default();
    let mut touched: HashSet<NaiveDate> = HashSet::new();
    {
        let mut get = tx
            .prepare_cached("SELECT day, json FROM oura_docs WHERE kind = ?1 AND id = ?2")
            .map_err(db)?;
        let mut put = tx
            .prepare_cached(
                "INSERT INTO oura_docs(kind, id, day, json, fetched_ts) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(kind, id) DO UPDATE SET
                   day = excluded.day, json = excluded.json, fetched_ts = excluded.fetched_ts",
            )
            .map_err(db)?;
        for d in docs.iter().filter(|d| d.kind == kind) {
            let old: Option<(String, String)> = get
                .query_row(params![kind, d.id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })
                .map_err(db)?;
            match &old {
                Some((day, json)) if *json == d.json && *day == fmt_date(d.day) => continue,
                Some((day, _)) => {
                    counts.updated += 1;
                    touched.extend(parse_date(day));
                }
                None => counts.added += 1,
            }
            touched.insert(d.day);
            put.execute(params![kind, d.id, fmt_date(d.day), d.json, fetched_ts])
                .map_err(db)?;
        }
    }
    if let Some((from, to)) = reconcile {
        let seen: HashSet<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        let mut st = tx
            .prepare("SELECT id, day FROM oura_docs WHERE kind = ?1 AND day >= ?2 AND day <= ?3")
            .map_err(db)?;
        let stale: Vec<(String, String)> = st
            .query_map(params![kind, fmt_date(from), fmt_date(to)], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)?
            .into_iter()
            .filter(|(id, _)| !seen.contains(id.as_str()))
            .collect();
        drop(st);
        for (id, day) in stale {
            tx.execute(
                "DELETE FROM oura_docs WHERE kind = ?1 AND id = ?2",
                params![kind, id],
            )
            .map_err(db)?;
            counts.removed += 1;
            touched.extend(parse_date(&day));
        }
    }
    if let (Some(lo), Some(hi)) = (touched.iter().min(), touched.iter().max()) {
        crate::rollup::rebuild(store, *lo, *hi)?;
    }
    tx.commit().map_err(db)?;
    Ok(counts)
}

/// The latest `day` stored for a collection, which is where the next sync
/// picks up (less `REFETCH_DAYS`).
pub fn last_day(store: &Store, kind: &str) -> Result<Option<NaiveDate>, String> {
    let d: Option<String> = store
        .conn
        .query_row(
            "SELECT MAX(day) FROM oura_docs WHERE kind = ?1",
            [kind],
            |r| r.get(0),
        )
        .map_err(|e| format!("db: {e}"))?;
    Ok(d.as_deref().and_then(parse_date))
}

/// The day span of everything stored, for a full rollup rebuild.
pub(crate) fn day_span(store: &Store) -> rusqlite::Result<Option<(NaiveDate, NaiveDate)>> {
    let (lo, hi): (Option<String>, Option<String>) =
        store
            .conn
            .query_row("SELECT MIN(day), MAX(day) FROM oura_docs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
    Ok(lo
        .as_deref()
        .and_then(parse_date)
        .zip(hi.as_deref().and_then(parse_date)))
}

/// One sync, as recorded in `oura_sync_log` and shown by `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncEntry {
    pub ts: i64,
    pub since: Option<String>,
    pub added: i64,
    pub updated: i64,
    pub removed: i64,
    /// Why the sync failed; null when it succeeded.
    pub error: Option<String>,
    pub elapsed_ms: i64,
}

pub fn log_sync(store: &Store, e: &SyncEntry) -> Result<(), String> {
    store
        .conn
        .execute(
            "INSERT INTO oura_sync_log(ts, since, added, updated, removed, error, elapsed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.ts,
                e.since,
                e.added,
                e.updated,
                e.removed,
                e.error,
                e.elapsed_ms
            ],
        )
        .map(|_| ())
        .map_err(|e| format!("db: {e}"))
}

/// The Oura part of `status`: stored documents, the last day covered, and
/// recent syncs, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OuraStatus {
    pub docs: i64,
    pub last_day: Option<String>,
    pub syncs: Vec<SyncEntry>,
}

pub fn status(store: &Store, limit: usize) -> Result<OuraStatus, String> {
    let db = |e: rusqlite::Error| format!("db: {e}");
    let c = &store.conn;
    let (docs, last_day): (i64, Option<String>) = c
        .query_row("SELECT COUNT(*), MAX(day) FROM oura_docs", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(db)?;
    let mut st = c
        .prepare(
            "SELECT ts, since, added, updated, removed, error, elapsed_ms
             FROM oura_sync_log ORDER BY id DESC LIMIT ?1",
        )
        .map_err(db)?;
    let syncs = st
        .query_map([limit as i64], |r| {
            Ok(SyncEntry {
                ts: r.get(0)?,
                since: r.get(1)?,
                added: r.get(2)?,
                updated: r.get(3)?,
                removed: r.get(4)?,
                error: r.get(5)?,
                elapsed_ms: r.get(6)?,
            })
        })
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)?;
    Ok(OuraStatus {
        docs,
        last_day,
        syncs,
    })
}

// ── daily metrics ─────────────────────────────────────────────────────────

/// (collection, JSON pointer, metric, scale). The value is the number at the
/// pointer times the scale (seconds → minutes for durations).
const FIELDS: &[(&str, &str, &str, f64)] = &[
    ("daily_readiness", "/score", "oura_readiness", 1.0),
    (
        "daily_readiness",
        "/temperature_deviation",
        "oura_temp_dev",
        1.0,
    ),
    ("daily_sleep", "/score", "oura_sleep_score", 1.0),
    ("daily_activity", "/score", "oura_activity_score", 1.0),
    ("daily_activity", "/steps", "oura_steps", 1.0),
    (
        "daily_activity",
        "/active_calories",
        "oura_active_kcal",
        1.0,
    ),
    (
        "daily_stress",
        "/stress_high",
        "oura_stress_high_min",
        1.0 / 60.0,
    ),
    (
        "daily_stress",
        "/recovery_high",
        "oura_recovery_high_min",
        1.0 / 60.0,
    ),
    (
        "daily_spo2",
        "/spo2_percentage/average",
        "oura_spo2_avg",
        1.0,
    ),
    (
        "daily_spo2",
        "/breathing_disturbance_index",
        "oura_bdi",
        1.0,
    ),
    ("vO2_max", "/vo2_max", "oura_vo2max", 1.0),
    (
        "daily_cardiovascular_age",
        "/vascular_age",
        "oura_cv_age",
        1.0,
    ),
];

/// Fields of the night's main `sleep` period.
const SLEEP_FIELDS: &[(&str, &str, f64)] = &[
    ("/total_sleep_duration", "oura_sleep_total_min", 1.0 / 60.0),
    ("/deep_sleep_duration", "oura_sleep_deep_min", 1.0 / 60.0),
    ("/rem_sleep_duration", "oura_sleep_rem_min", 1.0 / 60.0),
    ("/light_sleep_duration", "oura_sleep_light_min", 1.0 / 60.0),
    ("/awake_time", "oura_sleep_awake_min", 1.0 / 60.0),
    ("/efficiency", "oura_sleep_efficiency", 1.0),
    ("/lowest_heart_rate", "oura_hr_lowest", 1.0),
    ("/average_heart_rate", "oura_hr_avg_sleep", 1.0),
    ("/average_hrv", "oura_hrv_avg", 1.0),
    ("/average_breath", "oura_breath_avg", 1.0),
];

pub(crate) type Days = BTreeMap<NaiveDate, BTreeMap<&'static str, f64>>;

/// Add the `oura_*` metrics for `from..=to` to `out`. Each document counts
/// on Oura's own `day`. For `sleep`, the night is the longest `long_sleep`
/// period that day; naps and rest periods are left out.
pub(crate) fn rollup(
    store: &Store,
    from: NaiveDate,
    to: NaiveDate,
    out: &mut Days,
) -> Result<(), String> {
    let mut st = store
        .conn
        .prepare_cached(
            "SELECT kind, day, json FROM oura_docs WHERE day >= ?1 AND day <= ?2 ORDER BY kind, day, id",
        )
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map(params![fmt_date(from), fmt_date(to)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    // The main sleep period per day: (total sleep seconds, document).
    let mut nights: BTreeMap<NaiveDate, (f64, Value)> = BTreeMap::new();
    for (kind, day, json) in rows {
        let (Some(day), Ok(v)) = (parse_date(&day), serde_json::from_str::<Value>(&json)) else {
            continue;
        };
        match kind.as_str() {
            "sleep" => {
                if v.get("type").and_then(Value::as_str) != Some("long_sleep") {
                    continue;
                }
                let total = num(&v, "/total_sleep_duration").unwrap_or(0.0);
                if nights.get(&day).is_none_or(|(t, _)| total > *t) {
                    nights.insert(day, (total, v));
                }
            }
            _ => {
                for (k, ptr, metric, scale) in FIELDS {
                    if *k == kind {
                        if let Some(x) = num(&v, ptr) {
                            out.entry(day).or_default().insert(metric, x * scale);
                        }
                    }
                }
            }
        }
    }
    for (day, (_, v)) in nights {
        for (ptr, metric, scale) in SLEEP_FIELDS {
            if let Some(x) = num(&v, ptr) {
                out.entry(day).or_default().insert(metric, x * scale);
            }
        }
    }
    Ok(())
}

fn num(v: &Value, ptr: &str) -> Option<f64> {
    v.pointer(ptr)
        .and_then(Value::as_f64)
        .filter(|x| x.is_finite())
}

/// The fetch window for one collection: from `since` if given, else from
/// `REFETCH_DAYS` before the last stored day, else from `first` (a first
/// sync takes all history). Ends at `today`. Reconcile only the trailing
/// `REFETCH_DAYS` the fetch covered: nothing older is ever deleted.
pub fn window(
    since: Option<NaiveDate>,
    last: Option<NaiveDate>,
    first: NaiveDate,
    today: NaiveDate,
) -> (NaiveDate, NaiveDate, Option<(NaiveDate, NaiveDate)>) {
    let refetch_from = today - Duration::days(REFETCH_DAYS);
    let start = since
        .or(last.map(|d| d - Duration::days(REFETCH_DAYS)))
        .unwrap_or(first)
        .min(refetch_from);
    (start, today, Some((refetch_from.max(start), today)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        parse_date(s).unwrap()
    }

    fn page(kind: &str, docs: &[Value]) -> Vec<OuraDoc> {
        let body = serde_json::json!({ "data": docs, "next_token": null }).to_string();
        parse_page(kind, &body).unwrap().docs
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
    fn parse_rejects_missing_id_or_day() {
        let bad = r#"{"data":[{"day":"2024-01-02"}],"next_token":null}"#;
        assert!(parse_page("daily_sleep", bad).is_err());
        let bad = r#"{"data":[{"id":"a","day":"Jan 2"}],"next_token":null}"#;
        assert!(parse_page("daily_sleep", bad).is_err());
        assert!(parse_page("heartrate", r#"{"data":[]}"#).is_err());
        let ok = r#"{"data":[{"id":"a","day":"2024-01-02","score":80}],"next_token":"t2"}"#;
        let p = parse_page("daily_sleep", ok).unwrap();
        assert_eq!(p.next_token.as_deref(), Some("t2"));
        assert_eq!(p.docs[0].day, d("2024-01-02"));
    }

    #[test]
    fn apply_is_idempotent_and_updates_revisions() {
        let s = Store::open_in_memory().unwrap();
        let v1 = page(
            "daily_readiness",
            &[
                serde_json::json!({"id":"r1","day":"2024-01-02","score":70,"temperature_deviation":-0.2}),
            ],
        );
        let c = apply(&s, "daily_readiness", &v1, None, 1).unwrap();
        assert_eq!((c.added, c.updated, c.removed), (1, 0, 0));
        let c = apply(&s, "daily_readiness", &v1, None, 2).unwrap();
        assert_eq!((c.added, c.updated, c.removed), (0, 0, 0));
        assert_eq!(metric(&s, "2024-01-02", "oura_readiness"), Some(70.0));

        let v2 = page(
            "daily_readiness",
            &[serde_json::json!({"id":"r1","day":"2024-01-02","score":74})],
        );
        let c = apply(&s, "daily_readiness", &v2, None, 3).unwrap();
        assert_eq!(c.updated, 1);
        assert_eq!(metric(&s, "2024-01-02", "oura_readiness"), Some(74.0));
        assert_eq!(metric(&s, "2024-01-02", "oura_temp_dev"), None);
    }

    #[test]
    fn reconcile_deletes_only_inside_the_window() {
        let s = Store::open_in_memory().unwrap();
        let both = page(
            "daily_sleep",
            &[
                serde_json::json!({"id":"old","day":"2024-01-01","score":60}),
                serde_json::json!({"id":"new","day":"2024-01-09","score":80}),
            ],
        );
        apply(&s, "daily_sleep", &both, None, 1).unwrap();
        // A later fetch returns neither; only the one inside the window goes.
        let c = apply(
            &s,
            "daily_sleep",
            &[],
            Some((d("2024-01-05"), d("2024-01-10"))),
            2,
        )
        .unwrap();
        assert_eq!(c.removed, 1);
        assert_eq!(metric(&s, "2024-01-09", "oura_sleep_score"), None);
        assert_eq!(metric(&s, "2024-01-01", "oura_sleep_score"), Some(60.0));
    }

    #[test]
    fn sleep_takes_the_longest_long_sleep() {
        let s = Store::open_in_memory().unwrap();
        let docs = page(
            "sleep",
            &[
                serde_json::json!({"id":"a","day":"2024-01-02","type":"long_sleep",
                    "total_sleep_duration":25200,"deep_sleep_duration":3600,"average_hrv":41,
                    "lowest_heart_rate":50}),
                serde_json::json!({"id":"b","day":"2024-01-02","type":"long_sleep",
                    "total_sleep_duration":1800,"average_hrv":30}),
                serde_json::json!({"id":"c","day":"2024-01-02","type":"late_nap",
                    "total_sleep_duration":40000}),
            ],
        );
        apply(&s, "sleep", &docs, None, 1).unwrap();
        assert_eq!(
            metric(&s, "2024-01-02", "oura_sleep_total_min"),
            Some(420.0)
        );
        assert_eq!(metric(&s, "2024-01-02", "oura_sleep_deep_min"), Some(60.0));
        assert_eq!(metric(&s, "2024-01-02", "oura_hrv_avg"), Some(41.0));
        assert_eq!(metric(&s, "2024-01-02", "oura_hr_lowest"), Some(50.0));
    }

    #[test]
    fn nested_fields() {
        let s = Store::open_in_memory().unwrap();
        apply(
            &s,
            "daily_spo2",
            &page(
                "daily_spo2",
                &[serde_json::json!({"id":"y","day":"2024-01-02",
                    "spo2_percentage":{"average":96.5},"breathing_disturbance_index":3})],
            ),
            None,
            1,
        )
        .unwrap();
        assert_eq!(metric(&s, "2024-01-02", "oura_spo2_avg"), Some(96.5));
        assert_eq!(metric(&s, "2024-01-02", "oura_bdi"), Some(3.0));
    }

    #[test]
    fn every_metric_is_listed_in_rollup() {
        let ours = FIELDS
            .iter()
            .map(|f| f.2)
            .chain(SLEEP_FIELDS.iter().map(|f| f.1));
        for m in ours {
            assert!(
                crate::rollup::METRICS.contains(&m),
                "{m} missing from rollup::METRICS"
            );
        }
    }

    #[test]
    fn windows() {
        let today = d("2024-03-10");
        let first = d("2015-01-01");
        // First sync: all history, reconcile the trailing week.
        let (a, b, r) = window(None, None, first, today);
        assert_eq!((a, b, r), (first, today, Some((d("2024-03-03"), today))));
        // Later sync: a week before the last stored day.
        let (a, _, _) = window(None, Some(d("2024-03-09")), first, today);
        assert_eq!(a, d("2024-03-02"));
        // An old last day still refetches at least the trailing week.
        let (a, _, r) = window(None, Some(d("2024-01-01")), first, today);
        assert_eq!(a, d("2023-12-25"));
        assert_eq!(r, Some((d("2024-03-03"), today)));
        // --since inside the week narrows the reconcile window to match.
        let (a, _, r) = window(Some(d("2024-03-08")), None, first, today);
        assert_eq!(a, d("2024-03-03"));
        assert_eq!(r, Some((d("2024-03-03"), today)));
    }
}
