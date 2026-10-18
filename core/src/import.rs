//! Import an Apple Health export (`export.zip`, or an unpacked `export.xml`)
//! into the store. Idempotent: every row is keyed on its natural key and
//! inserted with `OR IGNORE`, so re-running an import — or importing a newer
//! export that overlaps an older one — adds only what is new. Rollups are
//! rebuilt afterwards for the span of days the new rows touched.

use chrono::{Duration, NaiveDate};
use rusqlite::params;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Instant;

use crate::health_export::{
    parse_export, parse_gpx, ActivityIn, Item, PointIn, RecordIn, WorkoutIn,
};
use crate::time::{local_date, sleep_night};
use crate::Store;

/// Rows per transaction. Big enough that commit overhead vanishes, small
/// enough that a crash loses little (and a re-run recovers it anyway).
const BATCH: usize = 50_000;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ImportReport {
    pub file: String,
    pub export_ts: Option<i64>,
    pub records_added: usize,
    pub records_skipped: usize,
    pub metadata_added: usize,
    pub workouts_added: usize,
    pub workouts_skipped: usize,
    pub routes_added: usize,
    pub route_points_added: usize,
    pub routes_missing: usize,
    pub activity_upserted: usize,
    pub days_rebuilt: usize,
    pub elapsed_ms: u64,
}

fn db<T>(r: rusqlite::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("db: {e}"))
}

struct Importer<'a, F: FnMut(&ImportReport)> {
    store: &'a Store,
    report: ImportReport,
    types: HashMap<String, i64>,
    sources: HashMap<String, i64>,
    devices: HashMap<String, i64>,
    in_batch: usize,
    /// (workout id, FileReference path) — resolved after the XML pass, since
    /// the zip can't hand out a second entry while export.xml is streaming.
    pending_routes: Vec<(i64, String)>,
    span: Option<(NaiveDate, NaiveDate)>,
    progress: F,
}

impl<'a, F: FnMut(&ImportReport)> Importer<'a, F> {
    fn new(store: &'a Store, file: String, progress: F) -> Self {
        Importer {
            store,
            report: ImportReport {
                file,
                ..Default::default()
            },
            types: HashMap::new(),
            sources: HashMap::new(),
            devices: HashMap::new(),
            in_batch: 0,
            pending_routes: Vec::new(),
            span: None,
            progress,
        }
    }

    fn touch(&mut self, d: NaiveDate) {
        self.span = Some(match self.span {
            None => (d, d),
            Some((lo, hi)) => (lo.min(d), hi.max(d)),
        });
    }

    fn tick(&mut self) -> Result<(), String> {
        self.in_batch += 1;
        if self.in_batch >= BATCH {
            db(self.store.conn.execute_batch("COMMIT; BEGIN"))?;
            self.in_batch = 0;
            (self.progress)(&self.report);
        }
        Ok(())
    }

    fn type_id(&mut self, identifier: &str, unit: Option<&str>) -> Result<i64, String> {
        if let Some(id) = self.types.get(identifier) {
            return Ok(*id);
        }
        let id = db(self.store.type_id(identifier, unit))?;
        self.types.insert(identifier.to_string(), id);
        Ok(id)
    }

    fn source_id(&mut self, name: &str) -> Result<i64, String> {
        if let Some(id) = self.sources.get(name) {
            return Ok(*id);
        }
        let id = db(self.store.source_id(name))?;
        self.sources.insert(name.to_string(), id);
        Ok(id)
    }

    fn device_id(&mut self, desc: Option<&str>) -> Result<Option<i64>, String> {
        let Some(desc) = desc else { return Ok(None) };
        if let Some(id) = self.devices.get(desc) {
            return Ok(Some(*id));
        }
        let id = db(self.store.device_id(desc))?;
        self.devices.insert(desc.to_string(), id);
        Ok(Some(id))
    }

    fn item(&mut self, item: Item) -> Result<(), String> {
        match item {
            Item::ExportDate(ts) => self.report.export_ts = Some(ts),
            Item::Record(r) => self.record(r)?,
            Item::Workout(w) => self.workout(w)?,
            Item::Activity(a) => self.activity(a)?,
        }
        Ok(())
    }

    fn record(&mut self, r: RecordIn) -> Result<(), String> {
        let type_id = self.type_id(&r.type_id, r.unit.as_deref())?;
        let source_id = self.source_id(&r.source)?;
        let device_id = self.device_id(r.device.as_deref())?;
        let conn = &self.store.conn;
        let n = db(conn
            .prepare_cached(
                "INSERT OR IGNORE INTO records(type_id, source_id, device_id, start_ts, end_ts,
                     tz_offset_min, created_ts, value_num, value_text, unit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .and_then(|mut st| {
                st.execute(params![
                    type_id,
                    source_id,
                    device_id,
                    r.start_ts,
                    r.end_ts,
                    r.tz_offset_min,
                    r.created_ts,
                    r.value_num,
                    r.value_text,
                    r.unit,
                ])
            }))?;
        if n == 0 {
            self.report.records_skipped += 1;
            return Ok(());
        }
        let id = conn.last_insert_rowid();
        if !r.metadata.is_empty() {
            let mut st = db(conn.prepare_cached(
                "INSERT OR IGNORE INTO record_metadata(record_id, key, value) VALUES (?1, ?2, ?3)",
            ))?;
            for (k, v) in &r.metadata {
                self.report.metadata_added += db(st.execute(params![id, k, v]))?;
            }
        }
        self.report.records_added += 1;
        self.touch(local_date(r.start_ts, r.tz_offset_min));
        self.touch(sleep_night(r.end_ts, r.tz_offset_min));
        self.tick()
    }

    fn workout(&mut self, w: WorkoutIn) -> Result<(), String> {
        let source_id = self.source_id(&w.source)?;
        let device_id = self.device_id(w.device.as_deref())?;
        let key = format!(
            "{}|{}|{}|{}",
            w.activity_type, w.start_ts, w.end_ts, w.source
        );
        let meta: BTreeMap<&str, &str> = w
            .metadata
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let meta_json = serde_json::to_string(&meta).map_err(|e| e.to_string())?;
        let conn = &self.store.conn;
        let n = db(conn.execute(
            "INSERT OR IGNORE INTO workouts(natural_key, activity_type, source_id, device_id,
                 start_ts, end_ts, tz_offset_min, duration_s, metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                key,
                w.activity_type,
                source_id,
                device_id,
                w.start_ts,
                w.end_ts,
                w.tz_offset_min,
                w.duration_s,
                meta_json
            ],
        ))?;
        if n == 0 {
            self.report.workouts_skipped += 1;
            return Ok(());
        }
        let id = conn.last_insert_rowid();
        for s in &w.stats {
            db(conn.execute(
                "INSERT OR IGNORE INTO workout_stats(workout_id, type, sum, avg, min, max, unit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, s.type_id, s.sum, s.avg, s.min, s.max, s.unit],
            ))?;
        }
        for e in &w.events {
            db(conn.execute(
                "INSERT INTO workout_events(workout_id, kind, ts, duration_s) VALUES (?1, ?2, ?3, ?4)",
                params![id, e.kind, e.ts, e.duration_s],
            ))?;
        }
        for f in w.route_files {
            self.pending_routes.push((id, f));
        }
        self.report.workouts_added += 1;
        self.touch(local_date(w.start_ts, w.tz_offset_min));
        self.tick()
    }

    fn activity(&mut self, a: ActivityIn) -> Result<(), String> {
        self.report.activity_upserted += db(self.store.conn.execute(
            "INSERT INTO activity_summaries(date, active_kcal, active_goal, move_min, move_goal,
                 exercise_min, exercise_goal, stand_hours, stand_goal)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(date) DO UPDATE SET
                 active_kcal = excluded.active_kcal, active_goal = excluded.active_goal,
                 move_min = excluded.move_min, move_goal = excluded.move_goal,
                 exercise_min = excluded.exercise_min, exercise_goal = excluded.exercise_goal,
                 stand_hours = excluded.stand_hours, stand_goal = excluded.stand_goal
             WHERE (active_kcal, active_goal, move_min, move_goal, exercise_min,
                    exercise_goal, stand_hours, stand_goal)
                IS NOT (excluded.active_kcal, excluded.active_goal, excluded.move_min,
                    excluded.move_goal, excluded.exercise_min, excluded.exercise_goal,
                    excluded.stand_hours, excluded.stand_goal)",
            params![
                a.date,
                a.active_kcal,
                a.active_goal,
                a.move_min,
                a.move_goal,
                a.exercise_min,
                a.exercise_goal,
                a.stand_hours,
                a.stand_goal
            ],
        ))?;
        Ok(())
    }

    fn route(&mut self, workout_id: i64, file: &str, points: &[PointIn]) -> Result<(), String> {
        let conn = &self.store.conn;
        let n = db(conn.execute(
            "INSERT OR IGNORE INTO routes(workout_id, file, point_count, distance_m, start_ts, end_ts)
             VALUES (?1, ?2, 0, NULL, NULL, NULL)",
            params![workout_id, file],
        ))?;
        if n == 0 {
            return Ok(());
        }
        let route_id = conn.last_insert_rowid();
        let mut added = 0usize;
        {
            let mut st = db(conn.prepare_cached(
                "INSERT OR IGNORE INTO route_points(route_id, ts, lat, lon, ele, speed, course, h_acc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            ))?;
            for p in points {
                added += db(st.execute(params![
                    route_id, p.ts, p.lat, p.lon, p.ele, p.speed, p.course, p.h_acc
                ]))?;
            }
        }
        let distance = crate::geo::path_length_m(points.iter().map(|p| (p.lat, p.lon)));
        db(conn.execute(
            "UPDATE routes SET point_count = ?2, distance_m = ?3, start_ts = ?4, end_ts = ?5
             WHERE id = ?1",
            params![
                route_id,
                added as i64,
                distance,
                points.first().map(|p| p.ts),
                points.last().map(|p| p.ts)
            ],
        ))?;
        self.report.routes_added += 1;
        self.report.route_points_added += added;
        self.in_batch += added;
        self.tick()
    }

    fn finish(mut self, started: Instant) -> Result<ImportReport, String> {
        db(self.store.conn.execute_batch("COMMIT; BEGIN"))?;
        db(self.store.refresh_type_stats())?;
        if let Some((lo, hi)) = self.span {
            // ±1 day: a sleep night or a sample near midnight can shift a
            // neighbouring day's totals.
            self.report.days_rebuilt =
                crate::rollup::rebuild(self.store, lo - Duration::days(1), hi + Duration::days(1))?;
        }
        self.report.elapsed_ms = started.elapsed().as_millis() as u64;
        let r = &self.report;
        db(self.store.conn.execute(
            "INSERT INTO import_log(ts, file, export_ts, records_added, records_skipped,
                 workouts_added, routes_added, route_points_added, activity_upserted,
                 days_rebuilt, elapsed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                chrono::Utc::now().timestamp(),
                r.file,
                r.export_ts,
                r.records_added as i64,
                r.records_skipped as i64,
                r.workouts_added as i64,
                r.routes_added as i64,
                r.route_points_added as i64,
                r.activity_upserted as i64,
                r.days_rebuilt as i64,
                r.elapsed_ms as i64
            ],
        ))?;
        db(self.store.conn.execute_batch("COMMIT"))?;
        Ok(self.report)
    }
}

/// Import from `path`: a `.zip` straight from the Health app, or an unpacked
/// `export.xml` (routes are then resolved relative to its directory).
/// `progress` is called after each committed batch.
pub fn import_path(
    store: &Store,
    path: &str,
    progress: impl FnMut(&ImportReport),
) -> Result<ImportReport, String> {
    let started = Instant::now();
    let file_name = Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let mut imp = Importer::new(store, file_name, progress);
    db(store.conn.execute_batch("BEGIN"))?;
    let result = if path.to_ascii_lowercase().ends_with(".zip") {
        import_zip(&mut imp, path)
    } else {
        import_xml(&mut imp, path)
    };
    match result {
        Ok(()) => imp.finish(started),
        Err(e) => {
            let _ = store.conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn import_zip<F: FnMut(&ImportReport)>(imp: &mut Importer<F>, path: &str) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| format!("{path}: {e}"))?;
    // The top-level folder name is localized ("apple_health_export",
    // "exportar", …); find export.xml by file name, not full path.
    let xml_name = zip
        .file_names()
        .find(|n| *n == "export.xml" || n.ends_with("/export.xml"))
        .map(str::to_string)
        .ok_or_else(|| format!("{path}: no export.xml inside"))?;
    let root = xml_name.trim_end_matches("export.xml").to_string();
    {
        let entry = zip.by_name(&xml_name).map_err(|e| e.to_string())?;
        parse_export(BufReader::with_capacity(1 << 20, entry), |item| {
            imp.item(item)
        })?;
    }
    for (workout_id, file) in std::mem::take(&mut imp.pending_routes) {
        let name = format!("{root}{}", file.trim_start_matches('/'));
        match zip.by_name(&name) {
            Ok(entry) => route_from(imp, workout_id, &file, entry)?,
            Err(_) => imp.report.routes_missing += 1,
        }
    }
    Ok(())
}

fn import_xml<F: FnMut(&ImportReport)>(imp: &mut Importer<F>, path: &str) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    parse_export(BufReader::with_capacity(1 << 20, file), |item| {
        imp.item(item)
    })?;
    let dir = Path::new(path).parent().unwrap_or(Path::new("."));
    for (workout_id, file) in std::mem::take(&mut imp.pending_routes) {
        match std::fs::File::open(dir.join(file.trim_start_matches('/'))) {
            Ok(f) => route_from(imp, workout_id, &file, f)?,
            Err(_) => imp.report.routes_missing += 1,
        }
    }
    Ok(())
}

fn route_from<F: FnMut(&ImportReport), R: Read>(
    imp: &mut Importer<F>,
    workout_id: i64,
    file: &str,
    input: R,
) -> Result<(), String> {
    let points = parse_gpx(BufReader::new(input)).map_err(|e| format!("{file}: {e}"))?;
    imp.route(workout_id, file, &points)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write the synthetic fixture as an unpacked export dir and as a zip.
    fn fixture_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vitals-import-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "_")
        ));
        let routes = dir.join("apple_health_export/workout-routes");
        std::fs::create_dir_all(&routes).unwrap();
        std::fs::write(
            dir.join("apple_health_export/export.xml"),
            include_str!("../../examples/export-sample.xml"),
        )
        .unwrap();
        std::fs::write(
            routes.join("route_sample.gpx"),
            include_str!("../../examples/route-sample.gpx"),
        )
        .unwrap();
        dir
    }

    fn write_zip(dir: &Path) -> String {
        use std::io::Write;
        let path = dir.join("export.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in [
            (
                "apple_health_export/export.xml",
                include_str!("../../examples/export-sample.xml"),
            ),
            (
                "apple_health_export/workout-routes/route_sample.gpx",
                include_str!("../../examples/route-sample.gpx"),
            ),
        ] {
            z.start_file(name, opts).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        path.to_string_lossy().into_owned()
    }

    fn count(s: &Store, table: &str) -> i64 {
        s.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn imports_zip_then_reimport_adds_nothing() {
        let dir = fixture_dir();
        let zip = write_zip(&dir);
        let s = Store::open_in_memory().unwrap();

        let r = import_path(&s, &zip, |_| {}).unwrap();
        assert_eq!(r.records_added, 19);
        assert_eq!(r.records_skipped, 1, "the in-file duplicate step sample");
        assert_eq!(r.workouts_added, 2);
        assert_eq!(r.routes_added, 1);
        assert_eq!(r.route_points_added, 3);
        assert_eq!(r.routes_missing, 0);
        assert_eq!(r.activity_upserted, 2);
        assert_eq!(r.metadata_added, 2);
        assert!(r.export_ts.is_some());
        assert!(r.days_rebuilt > 0);

        let again = import_path(&s, &zip, |_| {}).unwrap();
        assert_eq!(again.records_added, 0);
        assert_eq!(again.records_skipped, 20);
        assert_eq!(again.workouts_added, 0);
        assert_eq!(again.routes_added, 0);
        assert_eq!(again.days_rebuilt, 0);
        assert_eq!(
            again.activity_upserted, 0,
            "unchanged summaries are not rewritten"
        );
        assert_eq!(count(&s, "records"), 19);
        assert_eq!(count(&s, "route_points"), 3);
        assert_eq!(count(&s, "import_log"), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn imports_unpacked_xml_with_routes() {
        let dir = fixture_dir();
        let s = Store::open_in_memory().unwrap();
        let xml = dir.join("apple_health_export/export.xml");
        let r = import_path(&s, xml.to_str().unwrap(), |_| {}).unwrap();
        assert_eq!(r.records_added, 19);
        assert_eq!(r.routes_added, 1);
        let distance: f64 = s
            .conn
            .query_row("SELECT distance_m FROM routes", [], |r| r.get(0))
            .unwrap();
        assert!(distance > 100.0 && distance < 200.0, "{distance}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn type_stats_refreshed() {
        let dir = fixture_dir();
        let s = Store::open_in_memory().unwrap();
        import_path(&s, write_zip(&dir).as_str(), |_| {}).unwrap();
        let n: i64 = s
            .conn
            .query_row(
                "SELECT record_count FROM types WHERE identifier = 'HKQuantityTypeIdentifierStepCount'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 6);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_an_error() {
        let s = Store::open_in_memory().unwrap();
        assert!(import_path(&s, "/nonexistent/export.zip", |_| {}).is_err());
    }
}
