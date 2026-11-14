# Changelog

Every deploy of vitals is a release, versioned `vYYYY.MM.N` (N counts
from 0 each month). Each section is drafted from commit subjects by
git-cliff (`cliff.toml`) through `scripts/release.py`; big releases open
with a short summary. See `docs/RELEASING.md`.

## v2026.11.3 (2026-11-13)

### Features

- cli: Agent-wrapper conventions: `--json` before or after the subcommand prints one `{ok, ...}` or `{ok: false, error: {code, message}}` object, with typed exit codes (2 usage, 3 not found, ...) (ho-8ti) ([d92a6b4](https://github.com/dpkirschner/vitals/commit/d92a6b4))

## v2026.11.2 (2026-11-12)

Oura ring data, pulled automatically. With Oura configured (opt-in;
without it vitals still makes no network calls), `vitals oura auth`
connects an Oura account once over OAuth, and the server syncs every
6 hours, re-fetching the last 7 days because Oura revises documents.
The data lands as separate `oura_*` daily metrics in `/daily` and
`/metrics` (readiness, sleep stages and score, HRV, lowest HR, SpO2,
temperature deviation, steps, stress, VO2 max, cardiovascular age),
kept apart from the Apple Health records so a later export doesn't
double-count. A collection the authorization doesn't cover is skipped
and reported in `status`, not fatal. Schema v3: a rollback past this
release needs `--restore-db`.

### Features

- Automatic Oura ring sync via the Oura API v2: `vitals oura auth|sync`, server background sync and `/oura/callback`, `oura_*` metrics (ho-dtu) ([73f6515](https://github.com/dpkirschner/vitals/commit/73f6515))

### Docs

- Releasing, deploying a release, and Claude's release steps (ho-t8z.5) ([6cc7862](https://github.com/dpkirschner/vitals/commit/6cc7862))

## v2026.11.1 (2026-11-09)

### Features

- Deploy releases by tag from their own folder: `scripts/deploy.sh build|install|rollback|status` (or `make deploy-build` / `make deploy`) replace `make prod`, and launchd runs `<data dir>/releases/current` instead of the checkout. `GET /api/version`, `vitals --version` and `status` report the release (ho-t8z.4) ([b64246f](https://github.com/dpkirschner/vitals/commit/b64246f))

## v2026.11.0 (2026-11-09)

First tagged release: a baseline before deploys move to tagged
releases. Earlier history is in `git log`.

vitals is a local, encrypted archive of one person's Apple Health data.
`vitals import` reads the Health app's `export.zip` into SQLCipher
SQLite; the `vitals` CLI and a loopback JSON API (`vitals-server`, two
token tiers) read it back for the user and their agents.

What this release does:

- **Import**: idempotent ingest of records, workouts, routes and activity
  rings, with mass and volume units converted and unconvertible mixes
  refused.
- **Reads**: `status`, `types`, `records`, `series` (day / week / month
  with sum, avg, min, max or count), `daily` rollups (steps, energy,
  resting heart rate, HRV, sleep, workouts), `sleep` (nights by the
  morning they end, naps kept out of the next night), `activity` and
  `workouts`. Workout routes need the full token.
- **Backup**: `vitals backup --to PATH` writes a verified snapshot of the
  DB, still encrypted with the same key, safe while the server runs.
- **Repo tooling**: `make leakscan` and a pre-push/CI scan keep personal
  data out of this public repo, with rules for health data (device owner
  names, precise coordinates, profile values, unlabelled samples);
  `make release` cuts releases like this one.
