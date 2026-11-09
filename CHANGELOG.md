# Changelog

Every deploy of vitals is a release, versioned `vYYYY.MM.N` (N counts
from 0 each month). Each section is drafted from commit subjects by
git-cliff (`cliff.toml`) through `scripts/release.py`; big releases open
with a short summary. See `docs/RELEASING.md`.

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
