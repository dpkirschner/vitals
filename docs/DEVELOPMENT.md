# Development

Prereq: a stable Rust toolchain (`rust-toolchain.toml`).

```
make check
```

`make check` runs `cargo fmt --check`, clippy with `-D warnings` and every test.

```
make import
```

Imports `$(EXPORT)` (default `./export.zip`, which is gitignored) into `$(DB)` (default `~/vitals-dev.db`).

```
make serve
```

Serves `/api` on 127.0.0.1:8090 against `$(DB)`.

```
make smoke
```

End-to-end check against a real database (`scripts/smoke.sh`). It starts its own
server on port 8091 (`PORT=` to change it), so it never touches a running daemon,
and stops it on exit. It checks:

- every auth tier: 401, read-only token, full token, 403 on mutations, 403 on GPS points
- every route returns 200
- error codes: 400 and 404
- the JSON shapes
- `vitals --server` output is identical to local `--json`

It needs `db-key`, `api-token` and `api-token-ro` in the data directory
(`DEPLOYMENT.md` §1), plus `jq`. It exits non-zero on any failure.

`make import`, `make serve` and `make prod` build with the `encryption` feature,
so export `VITALS_DB_KEY_FILE=~/Library/"Application Support"/vitals/db-key` first.
The server refuses to start without a key. The CLI opens the DB as plaintext when
no key is set. Plain `cargo test` and `cargo run` builds are plaintext.

## Tests

- **Unit tests** live inline (`#[cfg(test)] mod tests`). There is no `tests/` directory, as in outflow.
- **Fixtures** are `examples/export-sample.xml` and `examples/route-sample.gpx`. They are **synthetic**, and each case they contain exists for a test:
  - an iPhone and a Watch both logging the same walk (the one-source rule)
  - an exact duplicate record
  - sleep stages from two sources crossing midnight
  - a sample recorded at −0500 (the offset rule)
  - a workout with a route
  - a legacy self-closing workout
  - two activity summaries
- **Import tests** write the fixture to a temp directory (unpacked and zipped) and import it into `Store::open_in_memory()`.
- **Server tests** drive the real router with `tower::ServiceExt::oneshot`. The auth tests use a stand-in router, the same way outflow's do.

Never commit real export data. `.gitignore` blocks it; see INVARIANTS #9.

## Agent quick reference

```
vitals types
vitals daily --since 2026-09-01 --metrics steps,resting_hr,sleep_asleep_min
vitals series HeartRateVariabilitySDNN --bucket week --since 2026-01-01
vitals sleep --since 2026-10-01
vitals workouts --activity running --limit 10
vitals workout <id>
```

Add `--json` to any of them, or `--server URL` with the client build.
