//! Streaming parser for Apple Health's `export.xml` (and its GPX routes).
//! External input is parsed at this boundary into plain domain structs; the
//! importer never sees XML. The file is >1 GB, so this is a pull parser that
//! hands each top-level item to a sink as soon as it closes — never a DOM.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use std::io::BufRead;

use crate::time::{parse_hk_date, parse_rfc3339};

/// One `<Record>`: a quantity sample (numeric `value`) or a category sample
/// (`value` is an enum string like `HKCategoryValueSleepAnalysisAsleepDeep`).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordIn {
    pub type_id: String,
    pub unit: Option<String>,
    pub source: String,
    pub device: Option<String>,
    pub start_ts: i64,
    pub end_ts: i64,
    pub tz_offset_min: i32,
    pub created_ts: Option<i64>,
    pub value_num: Option<f64>,
    pub value_text: Option<String>,
    pub metadata: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkoutStatIn {
    pub type_id: String,
    pub sum: Option<f64>,
    pub avg: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkoutEventIn {
    pub kind: String,
    pub ts: i64,
    pub duration_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkoutIn {
    pub activity_type: String,
    pub source: String,
    pub device: Option<String>,
    pub start_ts: i64,
    pub end_ts: i64,
    pub tz_offset_min: i32,
    pub duration_s: Option<f64>,
    pub metadata: Vec<(String, String)>,
    pub stats: Vec<WorkoutStatIn>,
    pub events: Vec<WorkoutEventIn>,
    /// `FileReference` paths as written in the export, e.g.
    /// `/workout-routes/route_2022-03-03_7.40am.gpx`.
    pub route_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ActivityIn {
    /// `YYYY-MM-DD` (the summary's own calendar day).
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

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    ExportDate(i64),
    Record(RecordIn),
    Workout(WorkoutIn),
    Activity(ActivityIn),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PointIn {
    pub ts: i64,
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
    pub speed: Option<f64>,
    pub course: Option<f64>,
    pub h_acc: Option<f64>,
}

type Attrs = Vec<(String, String)>;

fn attrs(e: &BytesStart) -> Result<Attrs, String> {
    let mut out = Vec::with_capacity(12);
    for a in e.attributes() {
        let a = a.map_err(|err| format!("bad attribute: {err}"))?;
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let val = a
            .unescape_value()
            .map_err(|err| format!("bad attribute value for {key}: {err}"))?
            .into_owned();
        out.push((key, val));
    }
    Ok(out)
}

fn get<'a>(a: &'a Attrs, k: &str) -> Option<&'a str> {
    a.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

fn num(a: &Attrs, k: &str) -> Option<f64> {
    get(a, k).and_then(|v| v.trim().parse().ok())
}

fn req<'a>(a: &'a Attrs, k: &str, what: &str) -> Result<&'a str, String> {
    get(a, k).ok_or_else(|| format!("{what} missing {k}"))
}

fn date(a: &Attrs, k: &str, what: &str) -> Result<(i64, i32), String> {
    let v = req(a, k, what)?;
    parse_hk_date(v).ok_or_else(|| format!("{what}: bad {k} {v:?}"))
}

/// `<<HKDevice: 0x8bd397a80>, name:Apple Watch, …>` → `name:Apple Watch, …`.
/// The leading object address changes between exports, so it must not leak
/// into anything compared across imports.
pub fn normalize_device(raw: &str) -> String {
    let s = raw.trim();
    let s = match s.find(">, ") {
        Some(i) if s.starts_with("<<HKDevice") => &s[i + 3..],
        _ => s,
    };
    s.trim_end_matches('>').trim().to_string()
}

fn seconds(value: f64, unit: Option<&str>) -> f64 {
    match unit {
        Some("min") => value * 60.0,
        Some("hr") | Some("h") => value * 3600.0,
        Some("ms") => value / 1000.0,
        _ => value,
    }
}

fn record(a: &Attrs) -> Result<RecordIn, String> {
    let type_id = req(a, "type", "Record")?.to_string();
    let (start_ts, tz_offset_min) = date(a, "startDate", "Record")?;
    let (end_ts, _) = date(a, "endDate", "Record")?;
    let raw = get(a, "value");
    // Quantity samples are numeric; category samples carry an enum string.
    // Anything else (e.g. HKDataTypeSleepDurationGoal) is numeric when it parses.
    let (value_num, value_text) = match raw {
        None => (None, None),
        Some(v) if type_id.starts_with("HKCategoryTypeIdentifier") => (None, Some(v.to_string())),
        Some(v) => match v.trim().parse::<f64>() {
            Ok(n) => (Some(n), None),
            Err(_) => (None, Some(v.to_string())),
        },
    };
    Ok(RecordIn {
        type_id,
        unit: get(a, "unit").map(str::to_string),
        source: get(a, "sourceName").unwrap_or("").to_string(),
        device: get(a, "device").map(normalize_device),
        start_ts,
        end_ts,
        tz_offset_min,
        created_ts: get(a, "creationDate").and_then(parse_hk_date).map(|d| d.0),
        value_num,
        value_text,
        metadata: Vec::new(),
    })
}

fn workout(a: &Attrs) -> Result<WorkoutIn, String> {
    let (start_ts, tz_offset_min) = date(a, "startDate", "Workout")?;
    let (end_ts, _) = date(a, "endDate", "Workout")?;
    let mut stats = Vec::new();
    // Older exports put totals on the element itself rather than in
    // <WorkoutStatistics>; fold them into the same shape.
    if let Some(d) = num(a, "totalDistance") {
        stats.push(WorkoutStatIn {
            type_id: "HKQuantityTypeIdentifierDistanceWalkingRunning".into(),
            sum: Some(d),
            avg: None,
            min: None,
            max: None,
            unit: get(a, "totalDistanceUnit").map(str::to_string),
        });
    }
    if let Some(e) = num(a, "totalEnergyBurned") {
        stats.push(WorkoutStatIn {
            type_id: "HKQuantityTypeIdentifierActiveEnergyBurned".into(),
            sum: Some(e),
            avg: None,
            min: None,
            max: None,
            unit: get(a, "totalEnergyBurnedUnit").map(str::to_string),
        });
    }
    Ok(WorkoutIn {
        activity_type: req(a, "workoutActivityType", "Workout")?.to_string(),
        source: get(a, "sourceName").unwrap_or("").to_string(),
        device: get(a, "device").map(normalize_device),
        start_ts,
        end_ts,
        tz_offset_min,
        duration_s: num(a, "duration").map(|d| seconds(d, get(a, "durationUnit"))),
        metadata: Vec::new(),
        stats,
        events: Vec::new(),
        route_files: Vec::new(),
    })
}

fn activity(a: &Attrs) -> Option<ActivityIn> {
    Some(ActivityIn {
        date: get(a, "dateComponents")?.to_string(),
        active_kcal: num(a, "activeEnergyBurned"),
        active_goal: num(a, "activeEnergyBurnedGoal"),
        move_min: num(a, "appleMoveTime"),
        move_goal: num(a, "appleMoveTimeGoal"),
        exercise_min: num(a, "appleExerciseTime"),
        exercise_goal: num(a, "appleExerciseTimeGoal"),
        stand_hours: num(a, "appleStandHours"),
        stand_goal: num(a, "appleStandHoursGoal"),
    })
}

/// Push a metadata pair, keeping the first value per key (the export repeats
/// a workout's MetadataEntry block after its route).
fn push_meta(meta: &mut Vec<(String, String)>, a: &Attrs) {
    if let (Some(k), Some(v)) = (get(a, "key"), get(a, "value")) {
        if !meta.iter().any(|(key, _)| key == k) {
            meta.push((k.to_string(), v.to_string()));
        }
    }
}

enum Open {
    None,
    Record(RecordIn),
    Workout { w: WorkoutIn, in_route: bool },
}

/// Stream `export.xml`, calling `sink` once per top-level item in file order.
/// Unknown elements are skipped. A sink error aborts the parse.
pub fn parse_export<R: BufRead>(
    input: R,
    mut sink: impl FnMut(Item) -> Result<(), String>,
) -> Result<(), String> {
    let mut reader = Reader::from_reader(input);
    let mut buf = Vec::with_capacity(4096);
    let mut open = Open::None;
    loop {
        let pos = reader.buffer_position();
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("xml error near byte {pos}: {e}"))?;
        match ev {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let is_empty = matches!(ev, Event::Empty(_));
                let name = e.name();
                match (name.as_ref(), &mut open) {
                    (b"Record", Open::None) => {
                        let r = record(&attrs(e)?)?;
                        if is_empty {
                            sink(Item::Record(r))?;
                        } else {
                            open = Open::Record(r);
                        }
                    }
                    (b"Workout", Open::None) => {
                        let w = workout(&attrs(e)?)?;
                        if is_empty {
                            sink(Item::Workout(w))?;
                        } else {
                            open = Open::Workout { w, in_route: false };
                        }
                    }
                    (b"ActivitySummary", Open::None) => {
                        if let Some(a) = activity(&attrs(e)?) {
                            sink(Item::Activity(a))?;
                        }
                    }
                    (b"ExportDate", Open::None) => {
                        if let Some((ts, _)) = get(&attrs(e)?, "value").and_then(parse_hk_date) {
                            sink(Item::ExportDate(ts))?;
                        }
                    }
                    (b"MetadataEntry", Open::Record(r)) => push_meta(&mut r.metadata, &attrs(e)?),
                    (b"MetadataEntry", Open::Workout { w, in_route }) => {
                        if !*in_route {
                            push_meta(&mut w.metadata, &attrs(e)?);
                        }
                    }
                    (b"WorkoutStatistics", Open::Workout { w, .. }) => {
                        let a = attrs(e)?;
                        if let Some(t) = get(&a, "type") {
                            w.stats.push(WorkoutStatIn {
                                type_id: t.to_string(),
                                sum: num(&a, "sum"),
                                avg: num(&a, "average"),
                                min: num(&a, "minimum"),
                                max: num(&a, "maximum"),
                                unit: get(&a, "unit").map(str::to_string),
                            });
                        }
                    }
                    (b"WorkoutEvent", Open::Workout { w, .. }) => {
                        let a = attrs(e)?;
                        if let (Some(t), Some((ts, _))) =
                            (get(&a, "type"), get(&a, "date").and_then(parse_hk_date))
                        {
                            w.events.push(WorkoutEventIn {
                                kind: t.to_string(),
                                ts,
                                duration_s: num(&a, "duration")
                                    .map(|d| seconds(d, get(&a, "durationUnit"))),
                            });
                        }
                    }
                    (b"WorkoutRoute", Open::Workout { in_route, .. }) => {
                        if !is_empty {
                            *in_route = true;
                        }
                    }
                    (b"FileReference", Open::Workout { w, .. }) => {
                        if let Some(p) = get(&attrs(e)?, "path") {
                            if !w.route_files.iter().any(|f| f == p) {
                                w.route_files.push(p.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::End(ref e) => match (e.name().as_ref(), &mut open) {
                (b"Record", Open::Record(_)) => {
                    if let Open::Record(r) = std::mem::replace(&mut open, Open::None) {
                        sink(Item::Record(r))?;
                    }
                }
                (b"WorkoutRoute", Open::Workout { in_route, .. }) => *in_route = false,
                (b"Workout", Open::Workout { .. }) => {
                    if let Open::Workout { w, .. } = std::mem::replace(&mut open, Open::None) {
                        sink(Item::Workout(w))?;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Parse a workout-route GPX file into track points. Points without a
/// parseable time or position are dropped.
pub fn parse_gpx<R: BufRead>(input: R) -> Result<Vec<PointIn>, String> {
    let mut reader = Reader::from_reader(input);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    // (lat, lon, time, [ele, speed, course, hAcc]) of the open <trkpt>.
    type Open = (f64, f64, Option<i64>, [Option<f64>; 4]);
    let mut cur: Option<Open> = None;
    let mut field: Option<usize> = None; // 0 ele, 1 speed, 2 course, 3 hAcc, 4 time
    let mut time_text = String::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("gpx error: {e}"))?
        {
            Event::Eof => break,
            Event::Start(ref e) => match e.name().as_ref() {
                b"trkpt" => {
                    let a = attrs(e)?;
                    cur = match (num(&a, "lat"), num(&a, "lon")) {
                        (Some(lat), Some(lon)) => Some((lat, lon, None, [None; 4])),
                        _ => None,
                    };
                }
                b"ele" => field = Some(0),
                b"speed" => field = Some(1),
                b"course" => field = Some(2),
                b"hAcc" => field = Some(3),
                b"time" => {
                    field = Some(4);
                    time_text.clear();
                }
                _ => field = None,
            },
            Event::Text(t) => {
                if let (Some(f), Some(p)) = (field, cur.as_mut()) {
                    let s = t.unescape().map_err(|e| format!("gpx text: {e}"))?;
                    if f == 4 {
                        time_text.push_str(&s);
                        p.2 = parse_rfc3339(&time_text);
                    } else {
                        p.3[f] = s.trim().parse().ok();
                    }
                }
            }
            Event::End(ref e) => {
                field = None;
                if e.name().as_ref() == b"trkpt" {
                    if let Some((lat, lon, Some(ts), [ele, speed, course, h_acc])) = cur.take() {
                        out.push(PointIn {
                            ts,
                            lat,
                            lon,
                            ele,
                            speed,
                            course,
                            h_acc,
                        });
                    }
                }
            }
            Event::Empty(ref e) if e.name().as_ref() == b"trkpt" => {
                cur = None; // a point with no <time> is unusable
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../../examples/export-sample.xml");
    const GPX: &str = include_str!("../../examples/route-sample.gpx");

    fn items() -> Vec<Item> {
        let mut out = Vec::new();
        parse_export(SAMPLE.as_bytes(), |i| {
            out.push(i);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn parses_every_item_kind() {
        let items = items();
        assert!(matches!(items[0], Item::ExportDate(_)));
        let records = items
            .iter()
            .filter(|i| matches!(i, Item::Record(_)))
            .count();
        let workouts: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Workout(w) => Some(w),
                _ => None,
            })
            .collect();
        let summaries = items
            .iter()
            .filter(|i| matches!(i, Item::Activity(_)))
            .count();
        assert!(records > 10);
        assert_eq!(workouts.len(), 2);
        assert_eq!(summaries, 2);
    }

    #[test]
    fn record_fields_and_nested_metadata() {
        let items = items();
        let hrv = items
            .iter()
            .find_map(|i| match i {
                Item::Record(r) if r.type_id.ends_with("HeartRateVariabilitySDNN") => Some(r),
                _ => None,
            })
            .unwrap();
        assert_eq!(hrv.value_num, Some(42.5));
        assert_eq!(hrv.unit.as_deref(), Some("ms"));
        assert_eq!(hrv.tz_offset_min, -420);
        assert_eq!(
            hrv.metadata,
            vec![("HKAlgorithmVersion".into(), "2".into())]
        );
        assert_eq!(
            hrv.device.as_deref(),
            Some("name:Apple Watch, manufacturer:Apple Inc., model:Watch")
        );
        let sleep = items
            .iter()
            .find_map(|i| match i {
                Item::Record(r) if r.type_id.ends_with("SleepAnalysis") => Some(r),
                _ => None,
            })
            .unwrap();
        assert_eq!(sleep.value_num, None);
        assert!(sleep
            .value_text
            .as_deref()
            .unwrap()
            .starts_with("HKCategoryValueSleepAnalysis"));
    }

    #[test]
    fn workout_children_and_deduped_metadata() {
        let items = items();
        let run = items
            .iter()
            .find_map(|i| match i {
                Item::Workout(w) if w.activity_type.ends_with("Running") => Some(w),
                _ => None,
            })
            .unwrap();
        assert_eq!(run.duration_s, Some(1800.0));
        assert_eq!(run.route_files, vec!["/workout-routes/route_sample.gpx"]);
        assert_eq!(run.events.len(), 2);
        assert_eq!(run.stats.len(), 2);
        // MetadataEntry repeats after the route block; route metadata is not
        // workout metadata.
        assert_eq!(
            run.metadata
                .iter()
                .filter(|(k, _)| k == "HKIndoorWorkout")
                .count(),
            1
        );
        assert!(!run
            .metadata
            .iter()
            .any(|(k, _)| k == "HKMetadataKeySyncIdentifier"));
        // Legacy totals on the element become stats.
        let walk = items
            .iter()
            .find_map(|i| match i {
                Item::Workout(w) if w.activity_type.ends_with("Walking") => Some(w),
                _ => None,
            })
            .unwrap();
        assert_eq!(walk.stats.len(), 2);
        assert_eq!(walk.duration_s, Some(1200.0));
    }

    #[test]
    fn device_address_is_stripped() {
        assert_eq!(
            normalize_device("<<HKDevice: 0xabc>, name:iPhone, model:iPhone>"),
            "name:iPhone, model:iPhone"
        );
        assert_eq!(normalize_device("plain"), "plain");
    }

    #[test]
    fn bad_dates_are_errors_not_panics() {
        let xml = r#"<HealthData><Record type="HKQuantityTypeIdentifierStepCount" sourceName="x" startDate="nope" endDate="nope" value="1"/></HealthData>"#;
        let err = parse_export(xml.as_bytes(), |_| Ok(())).unwrap_err();
        assert!(err.contains("startDate"), "{err}");
    }

    #[test]
    fn parses_gpx_points() {
        let pts = parse_gpx(GPX.as_bytes()).unwrap();
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0].lat, 47.6000);
        assert_eq!(pts[0].ele, Some(10.5));
        assert_eq!(pts[0].speed, Some(2.5));
        assert_eq!(pts[0].h_acc, Some(4.0));
        assert!(pts[1].ts > pts[0].ts);
    }
}
