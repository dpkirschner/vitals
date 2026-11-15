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
| `/status` | `limit` (imports and Oura syncs to show, default 5) | Record, workout, route and day counts; coverage `first_ts`/`last_ts`; recent imports; `oura: {docs, last_day, syncs:[{ts, since, added, updated, removed, error, elapsed_ms}]}`. |
| `/types` | — | `[{name, identifier, kind, unit, default_agg, count, first_ts, last_ts}]`, largest first. |
| `/metrics` | — | Names `/daily` accepts. |
| `/records` | `type`, `since`, `until` (epoch s), `source`, `limit` (≤1000, default 100), `offset` | `{items:[{id,type,source,start_ts,end_ts,tz_offset_min,value,value_text,unit}], total}`, newest first. |
| `/series` | `type` (required), `since`, `until` (epoch s), `bucket` = `day`\|`week`\|`month`, `agg` = `sum`\|`avg`\|`min`\|`max`\|`count` | `{type, unit, bucket, agg, points:[{bucket, value, samples}]}`. See the notes below. |
| `/daily` | `since`, `until` (`YYYY-MM-DD`, inclusive), `metrics` (comma list) | `[{date, metrics:{name: value}}]`. Missing metrics are absent, not zero. |
| `/sleep` | `since`, `until` (`YYYY-MM-DD`) | `[{date, start_ts, end_ts, asleep_min, in_bed_min, awake_min, deep_min, rem_min, core_min, nap_min}]`. `date` is the morning the night ends. `nap_min` is asleep time outside the main block, such as the previous afternoon's nap. |
| `/activity` | `since`, `until` (`YYYY-MM-DD`) | Activity ring values and goals for each day. |
| `/chart` | The embed chart params (**Embed views** below); nothing else is accepted | `{metric, step: day\|week\|month, mark: bar\|line, title, unit, per, range_label, points:[{key, value, highlighted}], partial_key, headline:{label, value, change, change_since}}`: one chart, `core::chart`. See the notes below. |
| `/workouts` | `activity` (`running`, `walking`, …), `since`, `until` (epoch s), `limit` | `[{id, activity, source, start_ts, end_ts, tz_offset_min, duration_min, distance_m, active_kcal, has_route}]`, newest first. |
| `/workouts/{id}` | — | The workout row plus `metadata`, `stats`, `events`, and `routes` summaries (`point_count`, `distance_m`; no coordinates). |
| `/workouts/{id}/route` | — | `[{ts, lat, lon, ele, speed, course, h_acc}]`. **Full token only.** |

One route lives outside `/api`: `GET /oura/callback` (with `--features oura`),
the page Oura sends the browser back to during `vitals oura auth`. It needs no
token, is static, never echoes or logs its query string (which holds the
one-time authorization code), and reads nothing from the archive.

Notes on `/series`:
- Values are converted to canonical units: m, kcal, min, mL, kg (body mass) and g (food weight); mg and mcg nutrients keep their unit. A type whose rows still disagree on unit after conversion returns an error instead of mixing them.
- `agg` defaults per type: `sum` for cumulative types, `avg` for levels, `count` for category types.
- `sum` takes the best single source per day, then adds the days up (INVARIANTS #2).
- Buckets are local dates. A `week` is labelled by its Monday.

Notes on `/chart`:
- `key` is the first day of the point's bucket (`YYYY-MM-DD`: the day, the week's Monday, the month's 1st). The step follows the range's length: days up to 92, weeks up to two years, months beyond. Training is never daily.
- `value` is in `unit`: weight in `lb` when the latest body mass record is in lb, else `kg`; sleep `h` asleep (`sleep_asleep_min`); `resting_hr` `bpm`; `hrv` `ms` (`hrv_sdnn_avg`); `steps`; training `min` of workouts (`workout_min`); `vo2max` `mL/kg·min`. A bucket averages its days that have a value, except training, which sums them (a day with none is 0). Bars keep empty buckets as `null`; lines leave them out.
- `partial_key`: the last bucket, when today is in it and the value is still growing (steps, training).
- `headline`: weight and VO2 max show the latest reading in the highlight (or range) with `change` since `change_since`, the first reading in it. Sleep, steps, resting HR and HRV show the average day over the highlight or range (steps leave today out). Training shows the highlighted total, else the average whole week (or month).
- An unknown, repeated or bad parameter is a `400` whose `error` starts with the parameter's name (`url: not a chart parameter …`).

## Embed views

Chrome-less pages another app can frame (agent-avatar shows them as
screens). Framing needs the app's origin in `VITALS_FRAME_ANCESTORS`
(DEPLOYMENT.md "Framing"). The page itself needs no token and holds no
data; on a server with tokens it asks for the read-only token once, inside
the frame, and keeps it.

`/embed/chart` (a page, not under `/api`) draws one chart from
`GET /api/chart`. Its parameters are a strict allow-list, with the same
range and highlight spellings as outflow's embed chart. A framing app
validates against this list and builds the URL itself, never passing a URL
or free text from an agent:

| Param | Values |
|---|---|
| `metric` (required) | `weight` · `sleep` · `resting_hr` · `hrv` · `steps` · `training` · `vo2max` |
| `range` | `7d` `30d` (days ending today) · `3m` (default) `6m` `12m` (calendar months ending with this one, so `3m` in November starts Sep 1) · `ytd` · `all` |
| `from` / `to` | instead of `range`: `YYYY-MM` or `YYYY-MM-DD`. A month `from` starts on its 1st, a month `to` ends on its last day, and `to` past today is today. Either may be left out (from the first day with data, through today) |
| `highlight` | a month `2026-10`, a day `2026-10-03`, or a span `2026-09..2026-10` (either spelling at each end). Must overlap the range. On week or month buckets a day lights its bucket |
| `theme` | `auto` (default, follows the device) · `light` · `dark`. Read by the page only |

Anything else, or a bad value, shows "Can't show this chart" with the
server's message naming the parameter; nothing is guessed. Any other path
under `/embed/` shows the same card.

For an agent-avatar screens config, the matching param kinds are:
`metric` a list of the metric names (or fix it in the screen's URL),
`range` a list, `from`/`to` `date_or_month`, `highlight` `span`.

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
