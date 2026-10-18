# vitals

Local Apple Health archive: Rust workspace (`core` / `cli` / `server`), modeled
on ~/code/outflow. Read `docs/INVARIANTS.md` before changing ingest, rollups or
auth.

- Gate: `make check` (fmt, clippy -D warnings, tests). Run it before committing.
- All reads go through `core/src/query.rs`. The CLI's local mode and the server must share it.
- Schema changes: append-only columns, bump `SCHEMA_VERSION` in `core/src/store.rs`, add a guarded block in `run_migrations`.
- Fixtures in `examples/` are synthetic. Never add real export data, DBs or GPX files to the repo.
- Real data for manual testing: `~/vitals-dev.db` (encrypted; `VITALS_DB_KEY_FILE=~/Library/"Application Support"/vitals/db-key`).
