# API

The server is `vitals-server`, listening on 127.0.0.1:8090 by default. Every route is
under `/api` and every route is a `GET`. Imports happen through the CLI on the host.

- **Auth**: send `Authorization: Bearer <token>` when tokens are configured (DEPLOYMENT.md §API tokens).
- **Tiers**:
  - The **full** token can read everything.
  - The **read-only** token can read everything **except** `/workouts/{id}/route`, which returns 403 (INVARIANTS #4).
- **Errors** are JSON `{"error": "…"}`:
  - 400 for a bad parameter (unknown type, bad bucket, unknown metric)
  - 404 for an unknown id or an unknown `/api` path
  - 500 for a database failure
- **Wire format**: snake_case fields, epoch-second timestamps (`*_ts`), local calendar dates as `YYYY-MM-DD`.
- **`type=` parameters** take the short name (`StepCount`, `HeartRate`), any capitalization, or the full HealthKit identifier.

## Endpoints

| Route | Params | Returns |
|---|---|---|
| `/status` | `limit` (imports to show, default 5) | Record, workout, route and day counts; coverage `first_ts`/`last_ts`; recent imports. |
| `/types` | — | `[{name, identifier, kind, unit, default_agg, count, first_ts, last_ts}]`, largest first. |
| `/metrics` | — | Names `/daily` accepts. |
| `/records` | `type`, `since`, `until` (epoch s), `source`, `limit` (≤1000, default 100), `offset` | `{items:[{id,type,source,start_ts,end_ts,tz_offset_min,value,value_text,unit}], total}`, newest first. |
| `/series` | `type` (required), `since`, `until` (epoch s), `bucket` = `day`\|`week`\|`month`, `agg` = `sum`\|`avg`\|`min`\|`max`\|`count` | `{type, unit, bucket, agg, points:[{bucket, value, samples}]}`. See the notes below. |
| `/daily` | `since`, `until` (`YYYY-MM-DD`, inclusive), `metrics` (comma list) | `[{date, metrics:{name: value}}]`. Missing metrics are absent, not zero. |
| `/sleep` | `since`, `until` (`YYYY-MM-DD`) | `[{date, start_ts, end_ts, asleep_min, in_bed_min, awake_min, deep_min, rem_min, core_min}]`. `date` is the morning the night ends. |
| `/activity` | `since`, `until` (`YYYY-MM-DD`) | Activity ring values and goals for each day. |
| `/workouts` | `activity` (`running`, `walking`, …), `since`, `until` (epoch s), `limit` | `[{id, activity, source, start_ts, end_ts, tz_offset_min, duration_min, distance_m, active_kcal, has_route}]`, newest first. |
| `/workouts/{id}` | — | The workout row plus `metadata`, `stats`, `events`, and `routes` summaries (`point_count`, `distance_m`; no coordinates). |
| `/workouts/{id}/route` | — | `[{ts, lat, lon, ele, speed, course, h_acc}]`. **Full token only.** |

Notes on `/series`:
- Values are converted to canonical units: m, kcal, min.
- `agg` defaults per type: `sum` for cumulative types, `avg` for levels, `count` for category types.
- `sum` takes the best single source per day, then adds the days up (INVARIANTS #2).
- Buckets are local dates. A `week` is labelled by its Monday.

## Examples

```
curl -s -H "Authorization: Bearer $VITALS_API_TOKEN" "$VITALS_SERVER/api/daily?since=2026-10-01&metrics=steps,resting_hr"
```

```
curl -s -H "Authorization: Bearer $VITALS_API_TOKEN" "$VITALS_SERVER/api/series?type=RestingHeartRate&bucket=month"
```

```
vitals --server "$VITALS_SERVER" sleep --since 2026-10-01
```

The CLI's `--since` and `--until` take `YYYY-MM-DD` everywhere. It converts them to
epoch-second bounds at local midnight (with `--until` covering that whole day)
for the epoch-based routes, and passes them through unchanged for the date-based ones.
`daily`, `sleep` and `activity` default to the last 30 days in the CLI. The API has no
default.
