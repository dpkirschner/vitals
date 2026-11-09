# vitals

Local Apple Health archive: Rust workspace (`core` / `cli` / `server`), modeled
on ~/code/outflow. Read `docs/INVARIANTS.md` before changing ingest, rollups or
auth.

- Gate: `make check` (fmt, clippy -D warnings, tests, `make leakscan`). Run it before committing.
- Leak scan: `scripts/leakscan.py` (shape rules incl. health ones: device owner names, precise coordinates, Me-record values, unlabelled `examples/` files; plus the private denylist via `LEAKSCAN_DENYLIST_FILE`). Exceptions go in `.leakscan-allow`; CI reruns it in `.github/workflows/leakscan.yml`. Shared with outflow's copy except the health rules: port fixes both ways.
- Integration testing: `make dev-db` rebuilds `~/vitals-dev.db` from synthetic data with edge cases (`scripts/synth-export.py`), then `make smoke` runs end to end on port 8091. Do this for API, ingest, rollup and unit changes. Extend `scripts/smoke.sh` for new routes, and the generator for new data shapes.
- Docs map: ARCHITECTURE (crates, data flow), DATA_MODEL (tables, metrics), INVARIANTS (rules), GOTCHAS (export quirks), API (endpoints, auth tiers), DEPLOYMENT (launchd, tokens, agent access), DEVELOPMENT (build, tests). Update the matching doc with any behaviour change.
- The repo is public: never commit personal details (names, hostnames, real paths) or data.
- All reads go through `core/src/query.rs`. The CLI's local mode and the server must share it.
- Schema changes: append-only columns, bump `SCHEMA_VERSION` in `core/src/store.rs`, add a guarded block in `run_migrations`.
- Fixtures in `examples/` are synthetic. Never add real export data, DBs or GPX files to the repo.
- Dev DB for manual testing: `~/vitals-dev.db`, synthetic via `make dev-db` (encrypted; `VITALS_DB_KEY_FILE=~/Library/"Application Support"/vitals/db-key`).
