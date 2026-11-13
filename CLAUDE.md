# vitals

Local Apple Health archive: Rust workspace (`core` / `oura` / `cli` / `server`), modeled
on ~/code/outflow. Read `docs/INVARIANTS.md` before changing ingest, rollups or
auth.

- Gate: `make check` (fmt, clippy -D warnings, tests, `make leakscan`). Run it before committing.
- Leak scan: `scripts/leakscan.py` (shape rules incl. health ones: device owner names, precise coordinates, Me-record values, unlabelled `examples/` files; plus the private denylist via `LEAKSCAN_DENYLIST_FILE`). Exceptions go in `.leakscan-allow`; CI reruns it in `.github/workflows/leakscan.yml`. Shared with outflow's copy except the health rules: port fixes both ways.
- Integration testing: `make dev-db` rebuilds `~/vitals-dev.db` from synthetic data with edge cases (`scripts/synth-export.py`), then `make smoke` runs end to end on port 8091. Do this for API, ingest, rollup and unit changes. Extend `scripts/smoke.sh` for new routes, and the generator for new data shapes.
- Docs map: ARCHITECTURE (crates, data flow), DATA_MODEL (tables, metrics), INVARIANTS (rules), GOTCHAS (export quirks), API (endpoints, auth tiers), DEPLOYMENT (launchd, deploying a release, tokens, agent access), DEVELOPMENT (build, tests), RELEASING (versions, releases, leak scan). Update the matching doc with any behaviour change.
- The repo is public: never commit personal details (names, hostnames, real paths) or data.
- Oura sync (opt-in, the only network code) lives in `oura/`; core stays network-free. Oura data is never merged into `records`; it surfaces as `oura_*` daily metrics.
- All reads go through `core/src/query.rs`. The CLI's local mode and the server must share it.
- Schema changes: append-only columns, bump `SCHEMA_VERSION` in `core/src/store.rs`, add a guarded block in `run_migrations`.
- Fixtures in `examples/` are synthetic. Never add real export data, DBs or GPX files to the repo.
- Dev DB for manual testing: `~/vitals-dev.db`, synthetic via `make dev-db` (encrypted; `VITALS_DB_KEY_FILE=~/Library/"Application Support"/vitals/db-key`).

## Releases and branches (see docs/RELEASING.md)

The user relies on Claude to drive releases and repo mechanics, as in outflow.
Every deploy is a release, versioned `vYYYY.MM.N` (N from 0 each month); notes
live in `CHANGELOG.md`.

- Work on a feature branch; merge into `main` locally when the change is whole
  (no PRs). Release from `main` only.
- "Cut a release": `make release-draft`, tidy the section (a big release gets a
  short summary paragraph), `make release` (make check + leak scan + commit +
  tag). Then **show the user the notes and ask once**: that yes covers pushing,
  `git push --atomic origin main V`, which publishes (the tag runs
  `.github/workflows/release.yml`). Afterwards confirm the workflow went green.
- Deploy = a published release. After the release workflow is green, run
  `scripts/deploy.sh build V` yourself (no sudo), then hand the user
  `scripts/deploy.sh install V` (sudo, their terminal; `--check SCRIPT` for a
  release-specific data check). Confirm with `scripts/deploy.sh status` or
  `GET /api/version`. Rollback: `scripts/deploy.sh rollback [V] [--restore-db]`
  (`--restore-db` past a schema bump). launchd runs
  `<data dir>/releases/current`, never this checkout, so building here doesn't
  touch the live server (docs/DEPLOYMENT.md).
- Never move or delete a pushed tag (a ruleset blocks it); fix forward with the
  next N. `main` blocks force-push; only for a history rewrite the user has
  approved does the ruleset get switched off, then back on.
- Pre-push leak scan: a block means fix the commits; a warning (dollar figure,
  unlabelled sample) means show the user and re-push with `LEAKSCAN_ACK=1` only
  after they say so. False shape-rule hits go in `.leakscan-allow`.
- The release, deploy and leak-scan scripts are copies of outflow's: port fixes
  both ways.
