//! Read queries shared by the CLI (local mode) and the server, so both emit
//! identical JSON shapes. Wire conventions match outflow: snake_case fields,
//! epoch-second timestamps, `YYYY-MM-DD` for local calendar dates.

use chrono::NaiveDate;
use rusqlite::{params, params_from_iter, OptionalExtension, ToSql};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::time::{bucket_label, local_date, Bucket};
use crate::units::{canonical, is_cumulative, short_activity, short_type};
use crate::Store;

/// Hard cap on page size for list endpoints (same as outflow's).
pub const MAX_LIMIT: usize = 1000;

fn db<T>(r: rusqlite::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("db: {e}"))
}

/// `db` for a chain that uses `?` on rusqlite results internally.
fn rows<T>(f: impl FnOnce() -> rusqlite::Result<T>) -> Result<T, String> {
    db(f())
}

// ── status / types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ImportEntry {
    pub ts: i64,
    pub file: String,
    pub export_ts: Option<i64>,
    pub records_added: i64,
    pub records_skipped: i64,
    pub workouts_added: i64,
    pub routes_added: i64,
    pub days_rebuilt: i64,
    pub elapsed_ms: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Status {
    pub records: i64,
    pub types: i64,
    pub workouts: i64,
    pub routes: i64,
    pub route_points: i64,
    pub days: i64,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub imports: Vec<ImportEntry>,
}

pub fn status(s: &Store, limit: usize) -> Result<Status, String> {
    let c = &s.conn;
    let n = |sql: &str| db(c.query_row(sql, [], |r| r.get::<_, i64>(0)));
    let (first_ts, last_ts) = db(c.query_row(
        "SELECT MIN(first_ts), MAX(last_ts) FROM types",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))?;
    let mut st = db(c.prepare(
        "SELECT ts, file, export_ts, records_added, records_skipped, workouts_added,
                routes_added, days_rebuilt, elapsed_ms
         FROM import_log ORDER BY id DESC LIMIT ?1",
    ))?;
    let imports = rows(|| {
        st.query_map([limit as i64], |r| {
            Ok(ImportEntry {
                ts: r.get(0)?,
                file: r.get(1)?,
                export_ts: r.get(2)?,
                records_added: r.get(3)?,
                records_skipped: r.get(4)?,
                workouts_added: r.get(5)?,
                routes_added: r.get(6)?,
                days_rebuilt: r.get(7)?,
                elapsed_ms: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
    })?;
    Ok(Status {
        records: n("SELECT COALESCE(SUM(record_count), 0) FROM types")?,
        types: n("SELECT COUNT(*) FROM types WHERE record_count > 0")?,
        workouts: n("SELECT COUNT(*) FROM workouts")?,
        routes: n("SELECT COUNT(*) FROM routes")?,
        route_points: n("SELECT COALESCE(SUM(point_count), 0) FROM routes")?,
        days: n("SELECT COUNT(DISTINCT date) FROM daily")?,
        first_ts,
        last_ts,
        imports,
    })
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct TypeInfo {
    /// Short name (`StepCount`) — what every `type=` parameter accepts.
    pub name: String,
    /// Full HealthKit identifier.
    pub identifier: String,
    pub kind: String,
    pub unit: Option<String>,
    /// Default aggregation for `series`: `sum` for cumulative types, `avg`
    /// for levels, `count` for category types.
    pub default_agg: String,
    pub count: i64,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
}

fn kind_of(identifier: &str) -> &'static str {
    if identifier.starts_with("HKCategoryTypeIdentifier") {
        "category"
    } else if identifier.starts_with("HKQuantityTypeIdentifier") {
        "quantity"
    } else {
        "other"
    }
}

fn default_agg(identifier: &str) -> Agg {
    if kind_of(identifier) == "category" {
        Agg::Count
    } else if is_cumulative(identifier) {
        Agg::Sum
    } else {
        Agg::Avg
    }
}

pub fn types(s: &Store) -> Result<Vec<TypeInfo>, String> {
    let mut st = db(s.conn.prepare(
        "SELECT identifier, unit, record_count, first_ts, last_ts FROM types
         WHERE record_count > 0 ORDER BY record_count DESC",
    ))?;
    let out = rows(|| {
        st.query_map([], |r| {
            let identifier: String = r.get(0)?;
            Ok(TypeInfo {
                name: short_type(&identifier).to_string(),
                kind: kind_of(&identifier).to_string(),
                default_agg: default_agg(&identifier).name().to_string(),
                identifier,
                unit: r.get(1)?,
                count: r.get(2)?,
                first_ts: r.get(3)?,
                last_ts: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
    })?;
    Ok(out)
}

/// Resolve a user-supplied type (`StepCount`, `stepcount`, or the full
/// identifier) to (id, identifier).
pub fn resolve_type(s: &Store, input: &str) -> Result<(i64, String), String> {
    let mut st = db(s.conn.prepare_cached("SELECT id, identifier FROM types"))?;
    let all = rows(|| {
        st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()
    })?;
    let want = input.trim();
    all.iter()
        .find(|(_, i)| i == want)
        .or_else(|| {
            all.iter()
                .find(|(_, i)| short_type(i).eq_ignore_ascii_case(want))
        })
        .cloned()
        .ok_or_else(|| format!("unknown type {want:?} — see `vitals types` / GET /api/types"))
}

// ── records ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Deserialize)]
pub struct RecordQuery {
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub source: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct RecordRow {
    pub id: i64,
    #[serde(rename = "type")]
    pub type_: String,
    pub source: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub tz_offset_min: i32,
    pub value: Option<f64>,
    pub value_text: Option<String>,
    pub unit: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
}

pub fn records(s: &Store, q: &RecordQuery) -> Result<Page<RecordRow>, String> {
    let mut wh = vec!["1=1".to_string()];
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(t) = &q.type_ {
        let (id, _) = resolve_type(s, t)?;
        wh.push("r.type_id = ?".into());
        args.push(Box::new(id));
    }
    if let Some(v) = q.since {
        wh.push("r.start_ts >= ?".into());
        args.push(Box::new(v));
    }
    if let Some(v) = q.until {
        wh.push("r.start_ts <= ?".into());
        args.push(Box::new(v));
    }
    if let Some(v) = &q.source {
        wh.push("s.name = ?".into());
        args.push(Box::new(v.clone()));
    }
    let wh = wh.join(" AND ");
    let from =
        "FROM records r JOIN types t ON t.id = r.type_id JOIN sources s ON s.id = r.source_id";
    let total = db(s.conn.query_row(
        &format!("SELECT COUNT(*) {from} WHERE {wh}"),
        params_from_iter(args.iter()),
        |r| r.get(0),
    ))?;
    let limit = q.limit.unwrap_or(100).min(MAX_LIMIT) as i64;
    let offset = q.offset.unwrap_or(0) as i64;
    let mut st = db(s.conn.prepare(&format!(
        "SELECT r.id, t.identifier, s.name, r.start_ts, r.end_ts, r.tz_offset_min,
                r.value_num, r.value_text, r.unit
         {from} WHERE {wh} ORDER BY r.start_ts DESC, r.id DESC LIMIT {limit} OFFSET {offset}"
    )))?;
    let items = rows(|| {
        st.query_map(params_from_iter(args.iter()), |r| {
            let identifier: String = r.get(1)?;
            Ok(RecordRow {
                id: r.get(0)?,
                type_: short_type(&identifier).to_string(),
                source: r.get(2)?,
                start_ts: r.get(3)?,
                end_ts: r.get(4)?,
                tz_offset_min: r.get(5)?,
                value: r.get(6)?,
                value_text: r.get(7)?,
                unit: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
    })?;
    Ok(Page { items, total })
}

// ── series ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

impl Agg {
    pub fn parse(s: &str) -> Option<Agg> {
        match s {
            "sum" => Some(Agg::Sum),
            "avg" => Some(Agg::Avg),
            "min" => Some(Agg::Min),
            "max" => Some(Agg::Max),
            "count" => Some(Agg::Count),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agg::Sum => "sum",
            Agg::Avg => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
            Agg::Count => "count",
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SeriesQuery {
    #[serde(rename = "type")]
    pub type_: String,
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// day | week | month (default day).
    pub bucket: Option<String>,
    /// sum | avg | min | max | count (default per type, see TypeInfo).
    pub agg: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct SeriesPoint {
    /// First local date of the bucket (`YYYY-MM-DD`).
    pub bucket: String,
    pub value: f64,
    pub samples: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Series {
    #[serde(rename = "type")]
    pub type_: String,
    pub unit: Option<String>,
    pub bucket: String,
    pub agg: String,
    pub points: Vec<SeriesPoint>,
}

pub fn series(s: &Store, q: &SeriesQuery) -> Result<Series, String> {
    let (type_id, identifier) = resolve_type(s, &q.type_)?;
    let bucket_name = q.bucket.as_deref().unwrap_or("day");
    let bucket = Bucket::parse(bucket_name)
        .ok_or_else(|| format!("bad bucket {bucket_name:?} (day|week|month)"))?;
    let agg = match q.agg.as_deref() {
        Some(a) => Agg::parse(a).ok_or_else(|| format!("bad agg {a:?} (sum|avg|min|max|count)"))?,
        None => default_agg(&identifier),
    };
    if kind_of(&identifier) == "category" && agg != Agg::Count {
        return Err(format!(
            "{} is a category type: only agg=count applies",
            short_type(&identifier)
        ));
    }
    let mut st = db(s.conn.prepare(
        "SELECT start_ts, tz_offset_min, value_num, unit, source_id FROM records
         WHERE type_id = ?1 AND start_ts >= ?2 AND start_ts <= ?3",
    ))?;
    let rows = rows(|| {
        st.query_map(
            params![
                type_id,
                q.since.unwrap_or(i64::MIN),
                q.until.unwrap_or(i64::MAX)
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i32>(1)?,
                    r.get::<_, Option<f64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()
    })?;

    let mut unit_out: Option<String> = None;
    // (local day, source) → sum, and bucket → overall (sum, n, min, max)
    let mut per_source: HashMap<(NaiveDate, i64), f64> = HashMap::new();
    let mut overall: BTreeMap<String, (f64, usize, f64, f64)> = BTreeMap::new();
    for (ts, off, v, unit, source) in rows {
        let day = local_date(ts, off);
        let label = bucket_label(day, bucket);
        let v = match (v, unit.as_deref()) {
            (Some(v), Some(u)) => {
                let (cv, cu) = canonical(v, u);
                unit_out.get_or_insert_with(|| cu.to_string());
                Some(cv)
            }
            (v, _) => v,
        };
        let o = overall
            .entry(label.clone())
            .or_insert((0.0, 0, f64::INFINITY, f64::NEG_INFINITY));
        o.1 += 1;
        if let Some(v) = v {
            o.0 += v;
            o.2 = o.2.min(v);
            o.3 = o.3.max(v);
            *per_source.entry((day, source)).or_default() += v;
        }
    }
    // Sum obeys the one-source rule (INVARIANTS #2) per *day* — the best
    // source can change from day to day — then days add up into the bucket,
    // so week/month sums equal the sum of the daily rollups.
    let mut best_day: HashMap<NaiveDate, f64> = HashMap::new();
    for ((day, _), sum) in per_source {
        let b = best_day.entry(day).or_insert(0.0);
        *b = b.max(sum);
    }
    let mut best: HashMap<String, f64> = HashMap::new();
    for (day, v) in best_day {
        *best.entry(bucket_label(day, bucket)).or_insert(0.0) += v;
    }
    let points = overall
        .into_iter()
        .map(|(label, (sum, n, min, max))| SeriesPoint {
            value: match agg {
                Agg::Sum => best.get(&label).copied().unwrap_or(0.0),
                Agg::Avg => sum / n as f64,
                Agg::Min => min,
                Agg::Max => max,
                Agg::Count => n as f64,
            },
            bucket: label,
            samples: n,
        })
        .collect();
    Ok(Series {
        type_: short_type(&identifier).to_string(),
        unit: if agg == Agg::Count { None } else { unit_out },
        bucket: bucket_name.to_string(),
        agg: agg.name().to_string(),
        points,
    })
}

// ── daily / sleep / activity ─────────────────────────────────────────────

#[derive(Debug, Default, Clone, Deserialize)]
pub struct DateRange {
    /// Inclusive `YYYY-MM-DD` bounds on the local date.
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct DailyRow {
    pub date: String,
    pub metrics: BTreeMap<String, f64>,
}

pub fn daily(s: &Store, r: &DateRange, metrics: &[String]) -> Result<Vec<DailyRow>, String> {
    for m in metrics {
        if !crate::rollup::METRICS.contains(&m.as_str()) {
            return Err(format!(
                "unknown metric {m:?}; one of: {}",
                crate::rollup::METRICS.join(", ")
            ));
        }
    }
    let mut st = db(s.conn.prepare(
        "SELECT date, metric, value FROM daily WHERE date >= ?1 AND date <= ?2 ORDER BY date",
    ))?;
    let rows = rows(|| {
        st.query_map(
            params![
                r.since.as_deref().unwrap_or("0000-00-00"),
                r.until.as_deref().unwrap_or("9999-99-99")
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()
    })?;
    let mut out: Vec<DailyRow> = Vec::new();
    for (date, metric, value) in rows {
        if !metrics.is_empty() && !metrics.contains(&metric) {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.date == date => {
                last.metrics.insert(metric, value);
            }
            _ => out.push(DailyRow {
                date,
                metrics: BTreeMap::from([(metric, value)]),
            }),
        }
    }
    Ok(out)
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Night {
    /// The local date the night ends on (noon→noon; INVARIANTS #3).
    pub date: String,
    pub start_ts: Option<i64>,
    pub end_ts: Option<i64>,
    pub asleep_min: Option<f64>,
    pub in_bed_min: Option<f64>,
    pub awake_min: Option<f64>,
    pub deep_min: Option<f64>,
    pub rem_min: Option<f64>,
    pub core_min: Option<f64>,
}

pub fn sleep(s: &Store, r: &DateRange) -> Result<Vec<Night>, String> {
    let wanted: Vec<String> = crate::rollup::METRICS
        .iter()
        .filter(|m| m.starts_with("sleep_"))
        .map(|m| m.to_string())
        .collect();
    Ok(daily(s, r, &wanted)?
        .into_iter()
        .filter(|d| !d.metrics.is_empty())
        .map(|d| {
            let m = |k: &str| d.metrics.get(k).copied();
            Night {
                start_ts: m("sleep_start").map(|v| v as i64),
                end_ts: m("sleep_end").map(|v| v as i64),
                asleep_min: m("sleep_asleep_min"),
                in_bed_min: m("sleep_in_bed_min"),
                awake_min: m("sleep_awake_min"),
                deep_min: m("sleep_deep_min"),
                rem_min: m("sleep_rem_min"),
                core_min: m("sleep_core_min"),
                date: d.date,
            }
        })
        .collect())
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ActivitySummary {
    pub date: String,
    pub active_kcal: Option<f64>,
    pub active_goal: Option<f64>,
    pub move_min: Option<f64>,
    pub move_goal: Option<f64>,
    pub exercise_min: Option<f64>,
    pub exercise_goal: Option<f64>,
    pub stand_hours: Option<f64>,
    pub stand_goal: Option<f64>,
}

pub fn activity(s: &Store, r: &DateRange) -> Result<Vec<ActivitySummary>, String> {
    let mut st = db(s.conn.prepare(
        "SELECT date, active_kcal, active_goal, move_min, move_goal, exercise_min,
                exercise_goal, stand_hours, stand_goal
         FROM activity_summaries WHERE date >= ?1 AND date <= ?2 ORDER BY date",
    ))?;
    let out = rows(|| {
        st.query_map(
            params![
                r.since.as_deref().unwrap_or("0000-00-00"),
                r.until.as_deref().unwrap_or("9999-99-99")
            ],
            |row| {
                Ok(ActivitySummary {
                    date: row.get(0)?,
                    active_kcal: row.get(1)?,
                    active_goal: row.get(2)?,
                    move_min: row.get(3)?,
                    move_goal: row.get(4)?,
                    exercise_min: row.get(5)?,
                    exercise_goal: row.get(6)?,
                    stand_hours: row.get(7)?,
                    stand_goal: row.get(8)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()
    })?;
    Ok(out)
}

// ── workouts / routes ────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Deserialize)]
pub struct WorkoutQuery {
    /// Short activity name, case-insensitive (`running`, `Walking`, …).
    pub activity: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct WorkoutRow {
    pub id: i64,
    pub activity: String,
    pub source: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub tz_offset_min: i32,
    pub duration_min: Option<f64>,
    pub distance_m: Option<f64>,
    pub active_kcal: Option<f64>,
    pub has_route: bool,
}

const WORKOUT_COLS: &str = "w.id, w.activity_type, s.name, w.start_ts, w.end_ts, w.tz_offset_min,
     w.duration_s, EXISTS(SELECT 1 FROM routes r WHERE r.workout_id = w.id)
     FROM workouts w JOIN sources s ON s.id = w.source_id";

fn workout_row(r: &rusqlite::Row) -> rusqlite::Result<WorkoutRow> {
    let activity: String = r.get(1)?;
    Ok(WorkoutRow {
        id: r.get(0)?,
        activity: short_activity(&activity).to_string(),
        source: r.get(2)?,
        start_ts: r.get(3)?,
        end_ts: r.get(4)?,
        tz_offset_min: r.get(5)?,
        duration_min: r.get::<_, Option<f64>>(6)?.map(|d| d / 60.0),
        distance_m: None,
        active_kcal: None,
        has_route: r.get(7)?,
    })
}

/// Fill distance / energy from the workout's statistics, canonicalized.
fn fill_totals(s: &Store, w: &mut WorkoutRow) -> Result<(), String> {
    let mut st = rows(|| {
        s.conn
            .prepare_cached("SELECT type, sum, unit FROM workout_stats WHERE workout_id = ?1")
    })?;
    let stats = rows(|| {
        st.query_map([w.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<f64>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
    })?;
    for (ty, sum, unit) in stats {
        let Some(sum) = sum else { continue };
        let v = unit.as_deref().map(|u| canonical(sum, u).0).unwrap_or(sum);
        if ty.starts_with("HKQuantityTypeIdentifierDistance") {
            w.distance_m = Some(w.distance_m.unwrap_or(0.0) + v);
        } else if ty == "HKQuantityTypeIdentifierActiveEnergyBurned" {
            w.active_kcal = Some(v);
        }
    }
    Ok(())
}

pub fn workouts(s: &Store, q: &WorkoutQuery) -> Result<Vec<WorkoutRow>, String> {
    let mut wh = vec!["1=1".to_string()];
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(a) = &q.activity {
        wh.push("lower(w.activity_type) = lower(?)".into());
        args.push(Box::new(format!("HKWorkoutActivityType{}", a.trim())));
    }
    if let Some(v) = q.since {
        wh.push("w.start_ts >= ?".into());
        args.push(Box::new(v));
    }
    if let Some(v) = q.until {
        wh.push("w.start_ts <= ?".into());
        args.push(Box::new(v));
    }
    let limit = q.limit.unwrap_or(100).min(MAX_LIMIT);
    let mut st = db(s.conn.prepare(&format!(
        "SELECT {WORKOUT_COLS} WHERE {} ORDER BY w.start_ts DESC LIMIT {limit}",
        wh.join(" AND ")
    )))?;
    let mut rows = rows(|| {
        st.query_map(params_from_iter(args.iter()), workout_row)?
            .collect::<Result<Vec<_>, _>>()
    })?;
    for w in &mut rows {
        fill_totals(s, w)?;
    }
    Ok(rows)
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct WorkoutStat {
    #[serde(rename = "type")]
    pub type_: String,
    pub sum: Option<f64>,
    pub avg: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct WorkoutEvent {
    pub kind: String,
    pub ts: i64,
    pub duration_s: Option<f64>,
}

/// Route summary only — no coordinates. Safe for the read-only token.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct RouteSummary {
    pub id: i64,
    pub point_count: i64,
    pub distance_m: Option<f64>,
    pub start_ts: Option<i64>,
    pub end_ts: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct WorkoutDetail {
    #[serde(flatten)]
    pub workout: WorkoutRow,
    pub metadata: BTreeMap<String, String>,
    pub stats: Vec<WorkoutStat>,
    pub events: Vec<WorkoutEvent>,
    pub routes: Vec<RouteSummary>,
}

pub fn workout(s: &Store, id: i64) -> Result<Option<WorkoutDetail>, String> {
    let row = db(s
        .conn
        .query_row(
            &format!("SELECT {WORKOUT_COLS} WHERE w.id = ?1"),
            [id],
            workout_row,
        )
        .optional())?;
    let Some(mut w) = row else { return Ok(None) };
    fill_totals(s, &mut w)?;
    let meta_json: String = db(s.conn.query_row(
        "SELECT metadata_json FROM workouts WHERE id = ?1",
        [id],
        |r| r.get(0),
    ))?;
    let metadata = serde_json::from_str(&meta_json).unwrap_or_default();
    let stats = rows(|| {
        s.conn
            .prepare(
                "SELECT type, sum, avg, min, max, unit FROM workout_stats WHERE workout_id = ?1",
            )?
            .query_map([id], |r| {
                let t: String = r.get(0)?;
                Ok(WorkoutStat {
                    type_: short_type(&t).to_string(),
                    sum: r.get(1)?,
                    avg: r.get(2)?,
                    min: r.get(3)?,
                    max: r.get(4)?,
                    unit: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
    })?;
    let events = rows(|| {
        s.conn
            .prepare(
                "SELECT kind, ts, duration_s FROM workout_events WHERE workout_id = ?1 ORDER BY ts",
            )?
            .query_map([id], |r| {
                let k: String = r.get(0)?;
                Ok(WorkoutEvent {
                    kind: k
                        .strip_prefix("HKWorkoutEventType")
                        .unwrap_or(&k)
                        .to_string(),
                    ts: r.get(1)?,
                    duration_s: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
    })?;
    let routes = rows(|| {
        s.conn
            .prepare(
                "SELECT id, point_count, distance_m, start_ts, end_ts FROM routes
             WHERE workout_id = ?1 ORDER BY id",
            )?
            .query_map([id], |r| {
                Ok(RouteSummary {
                    id: r.get(0)?,
                    point_count: r.get(1)?,
                    distance_m: r.get(2)?,
                    start_ts: r.get(3)?,
                    end_ts: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
    })?;
    Ok(Some(WorkoutDetail {
        workout: w,
        metadata,
        stats,
        events,
        routes,
    }))
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct RoutePoint {
    pub ts: i64,
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
    pub speed: Option<f64>,
    pub course: Option<f64>,
    pub h_acc: Option<f64>,
}

/// All route points for a workout (every route, in time order). `None` if the
/// workout does not exist; an empty list if it has no route.
pub fn route(s: &Store, workout_id: i64) -> Result<Option<Vec<RoutePoint>>, String> {
    let exists: bool = db(s.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM workouts WHERE id = ?1)",
        [workout_id],
        |r| r.get(0),
    ))?;
    if !exists {
        return Ok(None);
    }
    let pts = rows(|| {
        s.conn
            .prepare(
                "SELECT p.ts, p.lat, p.lon, p.ele, p.speed, p.course, p.h_acc
             FROM route_points p JOIN routes r ON r.id = p.route_id
             WHERE r.workout_id = ?1 ORDER BY p.ts",
            )?
            .query_map([workout_id], |r| {
                Ok(RoutePoint {
                    ts: r.get(0)?,
                    lat: r.get(1)?,
                    lon: r.get(2)?,
                    ele: r.get(3)?,
                    speed: r.get(4)?,
                    course: r.get(5)?,
                    h_acc: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
    })?;
    Ok(Some(pts))
}

/// Render route points as a GPX 1.1 track (for `vitals route --gpx`).
pub fn to_gpx(points: &[RoutePoint]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <gpx version=\"1.1\" creator=\"vitals\" xmlns=\"http://www.topografix.com/GPX/1/1\">\n\
         <trk><trkseg>\n",
    );
    for p in points {
        let time = chrono::DateTime::from_timestamp(p.ts, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "<trkpt lat=\"{}\" lon=\"{}\">{}<time>{time}</time></trkpt>\n",
            p.lat,
            p.lon,
            p.ele.map(|e| format!("<ele>{e}</ele>")).unwrap_or_default()
        ));
    }
    out.push_str("</trkseg></trk>\n</gpx>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::import_path;

    fn imported() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "vitals-query-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "_")
        ));
        let routes = dir.join("workout-routes");
        std::fs::create_dir_all(&routes).unwrap();
        let xml = dir.join("export.xml");
        std::fs::write(&xml, include_str!("../../examples/export-sample.xml")).unwrap();
        std::fs::write(
            routes.join("route_sample.gpx"),
            include_str!("../../examples/route-sample.gpx"),
        )
        .unwrap();
        let s = Store::open_in_memory().unwrap();
        import_path(&s, xml.to_str().unwrap(), |_| {}).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        s
    }

    #[test]
    fn status_and_types() {
        let s = imported();
        let st = status(&s, 5).unwrap();
        assert_eq!(st.records, 19);
        assert_eq!(st.workouts, 2);
        assert_eq!(st.route_points, 3);
        assert_eq!(st.imports.len(), 1);
        let t = types(&s).unwrap();
        assert_eq!(t[0].name, "SleepAnalysis"); // most rows first
        let steps = t.iter().find(|t| t.name == "StepCount").unwrap();
        assert_eq!(steps.count, 6);
        assert_eq!(steps.default_agg, "sum");
        let sleep = t.iter().find(|t| t.name == "SleepAnalysis").unwrap();
        assert_eq!(sleep.kind, "category");
        assert_eq!(sleep.default_agg, "count");
    }

    #[test]
    fn resolves_short_and_full_type_names() {
        let s = imported();
        let full = resolve_type(&s, "HKQuantityTypeIdentifierStepCount").unwrap();
        assert_eq!(resolve_type(&s, "stepcount").unwrap(), full);
        assert!(resolve_type(&s, "nope")
            .unwrap_err()
            .contains("unknown type"));
    }

    #[test]
    fn records_filter_and_paginate() {
        let s = imported();
        let p = records(
            &s,
            &RecordQuery {
                type_: Some("StepCount".into()),
                source: Some("Test Watch".into()),
                limit: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.total, 4);
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].value, Some(600.0)); // newest first
        assert_eq!(p.items[0].type_, "StepCount");
    }

    #[test]
    fn series_sum_uses_one_source_rule() {
        let s = imported();
        let sr = series(
            &s,
            &SeriesQuery {
                type_: "StepCount".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sr.agg, "sum");
        let day1 = sr.points.iter().find(|p| p.bucket == "2026-03-01").unwrap();
        assert_eq!(day1.value, 3500.0);
        assert_eq!(day1.samples, 4);
        let wk = series(
            &s,
            &SeriesQuery {
                type_: "StepCount".into(),
                bucket: Some("week".into()),
                ..Default::default()
            },
        )
        .unwrap();
        // 2026-03-01 is a Sunday (week of Feb 23); 03-03 and 03-04 are the next week.
        assert_eq!(wk.points.len(), 2);
        assert_eq!(wk.points[1].bucket, "2026-03-02");
        assert_eq!(wk.points[1].value, 1000.0);
        let dist = series(
            &s,
            &SeriesQuery {
                type_: "DistanceWalkingRunning".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dist.unit.as_deref(), Some("m"));
        assert_eq!(dist.points[0].value, 1500.0);
    }

    #[test]
    fn series_rejects_sum_of_category() {
        let s = imported();
        let err = series(
            &s,
            &SeriesQuery {
                type_: "SleepAnalysis".into(),
                agg: Some("sum".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("category"));
    }

    #[test]
    fn daily_and_sleep_views() {
        let s = imported();
        let r = DateRange {
            since: Some("2026-03-01".into()),
            until: Some("2026-03-02".into()),
        };
        let d = daily(&s, &r, &["steps".into()]).unwrap();
        assert_eq!(d.len(), 1, "no step samples on 03-02");
        assert_eq!(d[0].metrics["steps"], 3500.0);
        assert!(daily(&s, &r, &["bogus".into()]).is_err());
        let nights = sleep(&s, &r).unwrap();
        assert_eq!(nights.len(), 1);
        assert_eq!(nights[0].date, "2026-03-02");
        assert_eq!(nights[0].asleep_min, Some(450.0));
        let a = activity(&s, &r).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[1].exercise_min, Some(45.0));
    }

    #[test]
    fn workouts_detail_and_route() {
        let s = imported();
        let ws = workouts(&s, &WorkoutQuery::default()).unwrap();
        assert_eq!(ws.len(), 2);
        let run = workouts(
            &s,
            &WorkoutQuery {
                activity: Some("running".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(run.len(), 1);
        assert_eq!(run[0].activity, "Running");
        assert_eq!(run[0].distance_m, Some(5000.0));
        assert_eq!(run[0].active_kcal, Some(300.0));
        assert!(run[0].has_route);
        let d = workout(&s, run[0].id).unwrap().unwrap();
        assert_eq!(d.events.len(), 2);
        assert_eq!(d.events[0].kind, "Segment");
        assert_eq!(d.routes[0].point_count, 3);
        assert_eq!(d.metadata["HKTimeZone"], "America/Los_Angeles");
        let pts = route(&s, run[0].id).unwrap().unwrap();
        assert_eq!(pts.len(), 3);
        assert!(to_gpx(&pts).contains("<trkpt lat=\"47.6\""));
        assert!(route(&s, 9999).unwrap().is_none());
        assert!(workout(&s, 9999).unwrap().is_none());
    }
}
