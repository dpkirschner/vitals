# Data model

Everything lives in one SQLite database (SQLCipher in production). The schema
is in `core/src/store.rs` (`SCHEMA`), and migrations are keyed on
`PRAGMA user_version` (`SCHEMA_VERSION`, currently 3). Columns are append-only.
A migration may also recompute `daily` when a rollup rule changes: v2 rebuilt
it for the sleep-block rule, so the first open after upgrading takes longer.
v3 only added the two Oura tables.

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
| `oura_docs` | (`kind`, `id`) | One Oura document per row, as the API returned it: `kind` is the collection (`daily_readiness`, `sleep`, …), `id` Oura's id, `day` Oura's own `YYYY-MM-DD`, `json` the document, `fetched_ts`. Upserted (Oura revises documents); see INVARIANTS #1 for deletions. |
| `oura_sync_log` | `id` | One row per Oura sync: `since`, added / updated / removed, `error` (null when it worked), timing. `status` shows the latest. |

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
| `sleep_asleep_min`, `sleep_in_bed_min`, `sleep_awake_min`, `sleep_deep_min`, `sleep_rem_min`, `sleep_core_min` | `SleepAnalysis` | Union of intervals across sources, in the night's main block (#3). |
| `sleep_nap_min` | `SleepAnalysis` | Asleep minutes in the night's other blocks (naps), unioned (#3). |
| `sleep_start`, `sleep_end` | `SleepAnalysis` | Epoch seconds of the first asleep start and the last asleep end in the main block. |
| `workouts`, `workout_min` | `workouts` | Count and total minutes by local start date. |
| `oura_readiness`, `oura_temp_dev` | Oura `daily_readiness` | Score; temperature deviation (°C). |
| `oura_sleep_score` | Oura `daily_sleep` | Score. |
| `oura_activity_score`, `oura_steps`, `oura_active_kcal` | Oura `daily_activity` | As Oura reports them. |
| `oura_sleep_total_min`, `oura_sleep_deep_min`, `oura_sleep_rem_min`, `oura_sleep_light_min`, `oura_sleep_awake_min`, `oura_sleep_efficiency`, `oura_hr_lowest`, `oura_hr_avg_sleep`, `oura_hrv_avg`, `oura_breath_avg` | Oura `sleep` | The day's longest `long_sleep` period; naps and rest periods are left out. Durations in minutes. `oura_hrv_avg` is RMSSD, not comparable with Apple's SDNN. Oura's "light" is not Apple's "core". |
| `oura_stress_high_min`, `oura_recovery_high_min` | Oura `daily_stress` | Minutes. |
| `oura_resilience` | Oura `daily_resilience` | Level as 1 (limited) to 5 (exceptional). |
| `oura_spo2_avg`, `oura_bdi` | Oura `daily_spo2` | Average SpO2 %; breathing disturbance index. |
| `oura_vo2max`, `oura_cv_age` | Oura `vO2_max`, `daily_cardiovascular_age` | As Oura reports them (not every day). |

Every `oura_*` metric lands on Oura's own `day`, not on vitals' noon-to-noon
night rule, and is never merged with the Apple metric of a similar name.
