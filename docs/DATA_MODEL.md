# Data model

Everything lives in one SQLite database (SQLCipher in production). The schema
is in `core/src/store.rs` (`SCHEMA`), and migrations are keyed on
`PRAGMA user_version` (`SCHEMA_VERSION`, currently 1). Columns are append-only.

## Time

Every instant is stored as **epoch seconds** together with that sample's own
`tz_offset_min`, parsed from the export's `2022-02-27 19:45:35 -0700`. Local days come
from the sample's own offset, never from the machine's timezone (INVARIANTS #5).

## Tables

| Table | Key | Notes |
|---|---|---|
| `types` | `identifier` UNIQUE | Full HealthKit id plus the last-seen unit. Also holds denormalized `record_count`, `first_ts` and `last_ts`, refreshed after each import. |
| `sources` | `name` UNIQUE | e.g. a watch or phone name. Part of the record dedupe key. |
| `devices` | `description` UNIQUE | Normalized `HKDevice` string with its memory address stripped. Informational only. |
| `records` | natural key (below) | Quantity samples use `value_num`. Category samples (sleep stages, events) use `value_text`. Each keeps its own `unit`. |
| `record_metadata` | (`record_id`, `key`) | MetadataEntry pairs. HRV beat-to-beat lists are not stored. |
| `workouts` | `natural_key` = `activity\|start\|end\|source` | Includes `metadata_json` and `duration_s`. |
| `workout_stats` | (`workout_id`, `type`) | sum, avg, min and max with their unit. Legacy `totalDistance` and `totalEnergyBurned` attributes are folded in here. |
| `workout_events` | `id` | Segments, laps, pauses. |
| `routes` | (`workout_id`, `file`) | `point_count`, `distance_m` (haversine), start and end. |
| `route_points` | (`route_id`, `ts`) | lat, lon, ele, speed, course, h_acc. **Location history: full token only** over the API. |
| `activity_summaries` | `date` | Apple's ring values and goals for each day. Upserted when they change. |
| `daily` | (`date`, `metric`) | Derived and safe to rebuild (see below). |
| `import_log` | `id` | One row per import, holding its counts and timing. |

### Record natural key

The unique index `records_natural` is
`(type_id, start_ts, source_id, end_ts, IFNULL(value_num,''), IFNULL(value_text,''))`.

- The `IFNULL` is there because NULLs never collide in a SQLite UNIQUE index.
- The column order makes the index double as the `(type, time range)` index that every query uses.

## Daily metrics

`rollup::METRICS` lists every metric. `GET /api/metrics` returns the same list.

| Metric | From | Rule |
|---|---|---|
| `steps`, `distance_m`, `active_kcal`, `basal_kcal`, `flights`, `exercise_min` | the matching quantity types | Sum per source per day, then keep the **largest single source** (#2). |
| `resting_hr`, `hrv_sdnn_avg`, `walking_hr_avg`, `respiratory_rate_avg`, `vo2max` | level types | Mean of the day's samples. |
| `sleep_asleep_min`, `sleep_in_bed_min`, `sleep_awake_min`, `sleep_deep_min`, `sleep_rem_min`, `sleep_core_min` | `SleepAnalysis` | Union of intervals across sources, per night (#3). |
| `sleep_start`, `sleep_end` | `SleepAnalysis` | Epoch seconds of the first asleep start and the last asleep end that night. |
| `workouts`, `workout_min` | `workouts` | Count and total minutes by local start date. |
