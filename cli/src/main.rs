use chrono::{Duration, Local, NaiveDate, NaiveTime, TimeZone};
use clap::{Parser, Subcommand};
use outcome::{json_out, not_found, usage, CliResult, Fail};
use std::process::ExitCode;
use vitals_core::query::{self, DateRange, RecordQuery, RoutePoint, SeriesQuery, WorkoutQuery};
use vitals_core::time::fmt_date;
use vitals_core::{import_path, Store};

#[cfg(feature = "oura")]
mod oura;
mod outcome;
#[cfg(feature = "client")]
mod remote;

#[derive(Parser)]
#[command(
    name = "vitals",
    about = "Local Apple Health archive",
    version = vitals_core::build::VERSION.unwrap_or("unreleased build"),
)]
struct Cli {
    #[arg(long, env = "VITALS_DB", default_value = "vitals.db", global = true)]
    db: String,
    /// Run against a remote vitals-server instead of a local DB
    /// (needs --features client). Auth via VITALS_API_TOKEN if set.
    #[arg(long, env = "VITALS_SERVER", global = true)]
    server: Option<String>,
    /// Machine-readable JSON output (server mode always emits JSON).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Write a consistent, verified snapshot of the DB to a new file, still
    /// encrypted with the same key (safe while the server runs). Refuses to
    /// overwrite; the file is 0600. Local only.
    Backup {
        /// Where to write the snapshot (must not exist).
        #[arg(long)]
        to: String,
    },
    /// Oura ring: connect the account once, then sync (the server also syncs
    /// on its own). Needs the `oura` build feature and OURA_* settings.
    /// Local only.
    Oura {
        #[command(subcommand)]
        cmd: OuraCmd,
    },
    /// Import an Apple Health export: the `export.zip` from the Health app
    /// (Profile → Export All Health Data), or an unpacked `export.xml`.
    /// Idempotent — re-importing adds only new rows. Local only.
    Import { path: String },
    /// Archive size, coverage and recent imports.
    Status {
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// Every record type present, with counts, units and date span.
    Types,
    /// Raw samples, newest first.
    Records {
        /// Type short name (`HeartRate`) or full identifier.
        #[arg(long = "type")]
        type_: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// Source name exactly as `records` prints it (e.g. a watch or phone name).
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// One type aggregated per day / week / month.
    Series {
        /// Type short name (`StepCount`) or full identifier.
        #[arg(name = "TYPE")]
        type_: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value = "day", value_parser = ["day", "week", "month"])]
        bucket: String,
        /// Default depends on the type (see `types`).
        #[arg(long, value_parser = ["sum", "avg", "min", "max", "count"])]
        agg: Option<String>,
    },
    /// Daily rollups (steps, energy, resting HR, HRV, sleep, workouts, …).
    /// Defaults to the last 30 days.
    Daily {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// Comma-separated metric names (default: all).
        #[arg(long, value_delimiter = ',')]
        metrics: Vec<String>,
    },
    /// Per-night sleep totals and stages. Defaults to the last 30 nights.
    Sleep {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
    },
    /// Activity ring summaries. Defaults to the last 30 days.
    Activity {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
    },
    /// Workouts, newest first.
    Workouts {
        /// Activity short name, e.g. running, walking, cycling.
        #[arg(long)]
        activity: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// One workout: stats, events, metadata, route summary.
    Workout { id: i64 },
    /// A workout's GPS points (needs the full API token in server mode).
    Route {
        workout_id: i64,
        /// Emit GPX instead of a table / JSON.
        #[arg(long)]
        gpx: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum OuraCmd {
    /// Authorize vitals with Oura: open the printed URL, approve, then paste
    /// the address the browser lands on. Writes the 0600 token file.
    Auth,
    /// Fetch Oura documents now. Without --since: a week before the last
    /// stored day, or all history on the first run.
    Sync {
        /// Re-fetch from this day (YYYY-MM-DD).
        #[arg(long)]
        since: Option<String>,
    },
}

/// `YYYY-MM-DD` → epoch secs at local midnight (start of that day).
pub(crate) fn parse_since(s: &str) -> Result<i64, Fail> {
    let d =
        NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| usage(format!("bad date: {s}")))?;
    local_midnight(d)
}

/// `YYYY-MM-DD` → epoch secs at the last second of that day (inclusive bound).
pub(crate) fn parse_until(s: &str) -> Result<i64, Fail> {
    let d =
        NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| usage(format!("bad date: {s}")))?;
    Ok(local_midnight(d + Duration::days(1))? - 1)
}

fn local_midnight(d: NaiveDate) -> Result<i64, Fail> {
    Local
        .from_local_datetime(&d.and_time(NaiveTime::MIN))
        .earliest()
        .map(|t| t.timestamp())
        .ok_or_else(|| usage(format!("no local midnight on {d}")))
}

pub(crate) fn opt_ts(
    s: &Option<String>,
    f: fn(&str) -> Result<i64, Fail>,
) -> Result<Option<i64>, Fail> {
    s.as_deref().map(f).transpose()
}

/// Calendar-date range for daily/sleep/activity: validated, and defaulting to
/// the last 30 days when `--since` is omitted.
pub(crate) fn date_range(
    since: &Option<String>,
    until: &Option<String>,
) -> Result<DateRange, Fail> {
    for d in [since, until].into_iter().flatten() {
        NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| usage(format!("bad date: {d}")))?;
    }
    Ok(DateRange {
        since: Some(
            since
                .clone()
                .unwrap_or_else(|| fmt_date(Local::now().date_naive() - Duration::days(30))),
        ),
        until: until.clone(),
    })
}

/// SQLCipher key for the encrypted build: `VITALS_DB_KEY`, else
/// `VITALS_DB_KEY_FILE` (0600). Neither set → open as plaintext.
fn db_key() -> CliResult<Option<String>> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(k) = env("VITALS_DB_KEY") {
        return Ok(Some(k));
    }
    match env("VITALS_DB_KEY_FILE") {
        Some(p) => vitals_core::secrets::read_secret_file(&p)
            .map(Some)
            .map_err(|e| Fail::new(outcome::DENIED, e)),
        None => Ok(None),
    }
}

fn open_store(db: &str) -> CliResult<Store> {
    #[cfg(feature = "encryption")]
    if let Some(key) = db_key()? {
        return Store::open_encrypted(db, &key).map_err(|e| format!("open {db}: {e}").into());
    }
    Store::open(db).map_err(|e| format!("open {db}: {e}").into())
}

/// `backup` opens the DB itself, without migrating (see `core::backup`).
fn cmd_backup(db: &str, to: &str, json: bool) -> CliResult {
    let key = db_key()?;
    let r = vitals_core::backup::backup_db(db, to, key.as_deref())?;
    if json {
        return json_out(&r);
    }
    println!(
        "backed up {db} → {}: {:.1} MB, {}, schema v{}, {} tables, {} rows, integrity ok",
        r.path,
        r.bytes as f64 / 1_048_576.0,
        if r.encrypted {
            "encrypted"
        } else {
            "plaintext"
        },
        r.schema_version,
        r.tables.len(),
        r.total_rows()
    );
    Ok(())
}

/// A sample's wall-clock time at its own offset.
fn local_time(ts: i64, off: i32) -> String {
    chrono::FixedOffset::east_opt(off * 60)
        .and_then(|o| o.timestamp_opt(ts, 0).single())
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn machine_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn opt(v: Option<f64>, prec: usize) -> String {
    v.map(|v| format!("{v:.prec$}"))
        .unwrap_or_else(|| "-".into())
}

fn hm(min: Option<f64>) -> String {
    min.map(|m| format!("{}h{:02}", (m / 60.0) as i64, (m % 60.0).round() as i64))
        .unwrap_or_else(|| "-".into())
}

fn run(cli: Cli) -> CliResult {
    if let Some(server) = &cli.server {
        return run_remote(server, &cli.cmd);
    }
    if let Cmd::Backup { to } = &cli.cmd {
        return cmd_backup(&cli.db, to, cli.json);
    }
    if let Cmd::Oura { cmd } = &cli.cmd {
        #[cfg(feature = "oura")]
        return oura::run(&cli.db, cmd, cli.json, || open_store(&cli.db));
        #[cfg(not(feature = "oura"))]
        {
            let _ = cmd;
            return Err(usage(
                "this build has no Oura support (build with --features oura)",
            ));
        }
    }
    if let Cmd::Import { path } = &cli.cmd {
        if !std::path::Path::new(path).exists() {
            return Err(not_found(format!("{path}: no such file")));
        }
    }
    let store = open_store(&cli.db)?;
    match &cli.cmd {
        Cmd::Backup { .. } | Cmd::Oura { .. } => unreachable!("handled above"),
        Cmd::Import { path } => {
            let r = import_path(&store, path, |r| {
                eprint!(
                    "\r  {} records, {} workouts …",
                    r.records_added + r.records_skipped,
                    r.workouts_added + r.workouts_skipped
                );
            })?;
            eprintln!();
            if cli.json {
                return json_out(&r);
            }
            println!(
                "imported {}: +{} records ({} already present), +{} workouts, +{} routes ({} points{}), {} activity days, {} days rolled up in {:.1}s",
                r.file,
                r.records_added,
                r.records_skipped,
                r.workouts_added,
                r.routes_added,
                r.route_points_added,
                if r.routes_missing > 0 {
                    format!(", {} route files missing", r.routes_missing)
                } else {
                    String::new()
                },
                r.activity_upserted,
                r.days_rebuilt,
                r.elapsed_ms as f64 / 1000.0
            );
        }
        Cmd::Status { limit } => {
            let s = query::status(&store, *limit).map_err(outcome::query)?;
            if cli.json {
                return json_out(&s);
            }
            println!(
                "{} records across {} types, {} workouts, {} routes ({} points), {} days rolled up",
                s.records, s.types, s.workouts, s.routes, s.route_points, s.days
            );
            if let (Some(a), Some(b)) = (s.first_ts, s.last_ts) {
                println!("coverage: {} → {}", machine_time(a), machine_time(b));
            }
            println!(
                "version: {}",
                s.version.as_deref().unwrap_or("unreleased build")
            );
            for i in &s.imports {
                println!(
                    "  {}  {}  +{} records  +{} workouts  {:.1}s",
                    machine_time(i.ts),
                    i.file,
                    i.records_added,
                    i.workouts_added,
                    i.elapsed_ms as f64 / 1000.0
                );
            }
            if let Some(o) = s
                .oura
                .as_ref()
                .filter(|o| o.docs > 0 || !o.syncs.is_empty())
            {
                println!(
                    "oura: {} documents through {}",
                    o.docs,
                    o.last_day.as_deref().unwrap_or("-")
                );
                for e in &o.syncs {
                    let what = match &e.error {
                        Some(err) => format!("failed: {err}"),
                        None => format!("+{} ~{} -{}", e.added, e.updated, e.removed),
                    };
                    println!("  {}  oura sync  {what}", machine_time(e.ts));
                }
            }
        }
        Cmd::Types => {
            let t = query::types(&store).map_err(outcome::query)?;
            if cli.json {
                return json_out(&t);
            }
            for t in t {
                println!(
                    "{:<40} {:>9}  {:<10} {:<5}  {} → {}",
                    t.name,
                    t.count,
                    t.unit.unwrap_or_default(),
                    t.default_agg,
                    t.first_ts.map(machine_time).unwrap_or_default(),
                    t.last_ts.map(machine_time).unwrap_or_default()
                );
            }
        }
        Cmd::Records {
            type_,
            since,
            until,
            source,
            limit,
            offset,
        } => {
            let p = query::records(
                &store,
                &RecordQuery {
                    type_: type_.clone(),
                    since: opt_ts(since, parse_since)?,
                    until: opt_ts(until, parse_until)?,
                    source: source.clone(),
                    limit: Some(*limit),
                    offset: Some(*offset),
                },
            )
            .map_err(outcome::query)?;
            if cli.json {
                return json_out(&p);
            }
            for r in &p.items {
                let v = match (&r.value, &r.value_text) {
                    (Some(v), _) => format!("{v} {}", r.unit.as_deref().unwrap_or("")),
                    (None, Some(t)) => t.clone(),
                    _ => "-".into(),
                };
                println!(
                    "{}  {:<28} {:<24} {}",
                    local_time(r.start_ts, r.tz_offset_min),
                    r.type_,
                    v,
                    r.source
                );
            }
            println!("({} of {})", p.items.len(), p.total);
        }
        Cmd::Series {
            type_,
            since,
            until,
            bucket,
            agg,
        } => {
            let s = query::series(
                &store,
                &SeriesQuery {
                    type_: type_.clone(),
                    since: opt_ts(since, parse_since)?,
                    until: opt_ts(until, parse_until)?,
                    bucket: Some(bucket.clone()),
                    agg: agg.clone(),
                },
            )
            .map_err(outcome::query)?;
            if cli.json {
                return json_out(&s);
            }
            println!(
                "{} ({} per {}{})",
                s.type_,
                s.agg,
                s.bucket,
                s.unit
                    .as_deref()
                    .map(|u| format!(", {u}"))
                    .unwrap_or_default()
            );
            for p in &s.points {
                println!("  {}  {:>12.1}  ({} samples)", p.bucket, p.value, p.samples);
            }
        }
        Cmd::Daily {
            since,
            until,
            metrics,
        } => {
            let rows = query::daily(&store, &date_range(since, until)?, metrics)
                .map_err(outcome::query)?;
            if cli.json {
                return json_out(&rows);
            }
            for r in rows {
                let cols: Vec<String> = r
                    .metrics
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "sleep_start" | "sleep_end"))
                    .map(|(k, v)| format!("{k}={}", (v * 10.0).round() / 10.0))
                    .collect();
                println!("{}  {}", r.date, cols.join("  "));
            }
        }
        Cmd::Sleep { since, until } => {
            let nights =
                query::sleep(&store, &date_range(since, until)?).map_err(outcome::query)?;
            if cli.json {
                return json_out(&nights);
            }
            println!("night       asleep  in bed  deep   rem    core   awake  nap    bed → wake");
            for n in nights {
                println!(
                    "{}  {:<6}  {:<6}  {:<5}  {:<5}  {:<5}  {:<5}  {:<5}  {} → {}",
                    n.date,
                    hm(n.asleep_min),
                    hm(n.in_bed_min),
                    hm(n.deep_min),
                    hm(n.rem_min),
                    hm(n.core_min),
                    hm(n.awake_min),
                    hm(n.nap_min),
                    n.start_ts.map(machine_time).unwrap_or_default(),
                    n.end_ts.map(machine_time).unwrap_or_default()
                );
            }
        }
        Cmd::Activity { since, until } => {
            let rows =
                query::activity(&store, &date_range(since, until)?).map_err(outcome::query)?;
            if cli.json {
                return json_out(&rows);
            }
            println!("date        move (kcal)    exercise (min)  stand (h)");
            for a in rows {
                println!(
                    "{}  {:>5}/{:<6}  {:>5}/{:<8}  {:>3}/{:<3}",
                    a.date,
                    opt(a.active_kcal, 0),
                    opt(a.active_goal, 0),
                    opt(a.exercise_min, 0),
                    opt(a.exercise_goal, 0),
                    opt(a.stand_hours, 0),
                    opt(a.stand_goal, 0)
                );
            }
        }
        Cmd::Workouts {
            activity,
            since,
            until,
            limit,
        } => {
            let ws = query::workouts(
                &store,
                &WorkoutQuery {
                    activity: activity.clone(),
                    since: opt_ts(since, parse_since)?,
                    until: opt_ts(until, parse_until)?,
                    limit: Some(*limit),
                },
            )
            .map_err(outcome::query)?;
            if cli.json {
                return json_out(&ws);
            }
            for w in ws {
                println!(
                    "#{:<5} {}  {:<22} {:>6} min  {:>7} km  {:>5} kcal{}",
                    w.id,
                    local_time(w.start_ts, w.tz_offset_min),
                    w.activity,
                    opt(w.duration_min, 0),
                    opt(w.distance_m.map(|m| m / 1000.0), 2),
                    opt(w.active_kcal, 0),
                    if w.has_route { "  [route]" } else { "" }
                );
            }
        }
        Cmd::Workout { id } => {
            let w = query::workout(&store, *id)
                .map_err(outcome::query)?
                .ok_or_else(|| not_found(format!("no workout #{id}")))?;
            if cli.json {
                return json_out(&w);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&w).map_err(|e| e.to_string())?
            );
        }
        Cmd::Route { workout_id, gpx } => {
            let pts = query::route(&store, *workout_id)
                .map_err(outcome::query)?
                .ok_or_else(|| not_found(format!("no workout #{workout_id}")))?;
            print_route(&pts, *gpx, cli.json)?;
        }
    }
    Ok(())
}

pub(crate) fn print_route(pts: &[RoutePoint], gpx: bool, json: bool) -> CliResult {
    if gpx {
        print!("{}", query::to_gpx(pts));
    } else if json {
        json_out(&pts)?;
    } else {
        for p in pts {
            println!(
                "{}  {:.6},{:.6}  ele {}  {} m/s",
                machine_time(p.ts),
                p.lat,
                p.lon,
                opt(p.ele, 1),
                opt(p.speed, 1)
            );
        }
    }
    Ok(())
}

#[cfg(feature = "client")]
fn run_remote(server: &str, cmd: &Cmd) -> CliResult {
    let r = remote::Remote::new(server);
    if let Cmd::Route {
        workout_id,
        gpx: true,
    } = cmd
    {
        let body = r.get(&format!("/workouts/{workout_id}/route"))?;
        let pts: Vec<RoutePoint> =
            serde_json::from_str(&body).map_err(|e| format!("bad route JSON: {e}"))?;
        return print_route(&pts, true, false);
    }
    println!("{}", outcome::envelope(&remote::dispatch(&r, cmd)?));
    Ok(())
}

#[cfg(not(feature = "client"))]
fn run_remote(_server: &str, _cmd: &Cmd) -> CliResult {
    Err(usage("--server needs a build with `--features client`"))
}

fn main() -> ExitCode {
    let cli = match outcome::parse::<Cli>() {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    // Server mode always speaks JSON, failures included (a GPX route aside).
    let json = cli.json || cli.server.is_some();
    outcome::finish(json, run(cli))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_is_inclusive_of_the_whole_day() {
        let a = parse_since("2026-03-01").unwrap();
        let b = parse_until("2026-03-01").unwrap();
        // 23h or 25h on DST days, minus the final second.
        assert!((23 * 3600 - 1..25 * 3600).contains(&(b - a)));
        assert!(parse_since("03/01/2026").is_err());
    }

    #[test]
    fn date_range_defaults_and_validates() {
        let r = date_range(&None, &None).unwrap();
        assert!(r.since.is_some());
        assert!(r.until.is_none());
        assert!(date_range(&Some("nope".into()), &None).is_err());
    }

    #[test]
    fn hm_formats_minutes() {
        assert_eq!(hm(Some(450.0)), "7h30");
        assert_eq!(hm(None), "-");
    }
}
