# Invariants

Rules the code relies on. Break one only on purpose, and update this file when you do.

1. **Ingest is idempotent.** Every table is written with `INSERT OR IGNORE`, or
   with an upsert that changes nothing when values are equal, keyed on a natural
   key. Re-importing the same export, or a newer export that overlaps an older one,
   adds only new rows. Never key on anything that changes between exports: the
   `HKDevice` memory address is stripped for exactly this reason.
2. **No double counting across sources.** An iPhone and an Apple Watch both record
   the same steps, distance and energy. For additive metrics, sum per
   (local day, source) and keep the largest single source for each day. Week and
   month sums add up those per-day bests. Never add sources together. This
   applies in `rollup` and in `query::series` with `agg=sum`.
3. **Sleep is per night and unioned.** A sleep sample belongs to the local date
   it ends on, where days run noon to noon (`time::sleep_night`). Overlapping
   intervals, from one source or several, are unioned, never summed.
4. **Raw GPS is full-token only.** `GET /api/workouts/{id}/route` returns precise
   location history. `require_bearer` refuses it to the read-only token
   (`full_token_only`), while workout summaries, including route distance and
   point count, stay readable. This is the one route-specific rule in an
   otherwise method-based tier.
5. **Local day means the sample's own offset.** Bucketing uses each record's
   `tz_offset_min`, never the machine's timezone. A step sample taken at
   00:30 −0500 on the 4th counts toward the 4th, even if the server is in
   California.
6. **Secrets never touch the DB or argv.** They come from env or a 0600 file
   (`*_FILE`), and production uses `_FILE` only. A `_FILE` that is set but
   unreadable is fatal: the server fails closed.
7. **Core has no network code.** `vitals-core` reads files and SQLite only.
8. **One read path.** All reads go through `core::query`, so the CLI's local
   `--json` output and the API never drift apart.
9. **Real export data never enters the repo.** `.gitignore` blocks `*.zip`,
   `export*.xml`, `*.gpx` and `*.db`. Test fixtures in `examples/` are synthetic.
