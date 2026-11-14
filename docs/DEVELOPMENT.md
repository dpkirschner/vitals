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
make dev-db
```

Rebuilds `$(DB)` from synthetic data: `scripts/synth-export.py` writes a fake
export to a temp directory, and it's imported with the encrypted release CLI.
This is the integration-test setup. Run it, then `make smoke`, to test a
change against realistic data without touching real records. Defaults to 240
days ending yesterday; `DAYS=`, `SEED=` and `END=YYYY-MM-DD` change that, and
the same values always give the same data. It deletes `$(DB)` first, so it only
accepts a DB whose file name contains `dev`. The generator refuses to write
inside the repo.

The data is shaped to hit known edge cases. The script's docstring lists them:
two sources for steps, duplicates, a km→mi switch, kg then lb, water in mL and
fl oz, energy in Cal, kcal and kJ, glucose that switches from mg/dL to mmol/L
(no conversion), DST and a timezone trip, legacy and staged sleep, naps, a
late night ending after noon, routes, legacy workouts, and a workout crossing
midnight. When a bug turns out to need a new shape of data, add it there.

Oura data arrives the real way: `scripts/oura-mock.py` serves synthetic
documents for the same days on a loopback port (with fake tokens, already
expired so the sync refreshes first), and the release CLI runs
`vitals oura sync` against it via `OURA_API_BASE`. The mock pages 25 at a
time, leaves out readiness one day a week (ring off), and adds a nap beside
some nights.

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
- `vitals --server` output is identical to local `--json` (both enveloped)
- the `oura_*` metrics and `status.oura` from the mock sync, and that
  `/oura/callback` needs no token and neither shows nor logs its code (the
  smoke server logs at debug for this)

It needs `db-key`, `api-token` and `api-token-ro` in the data directory
(`DEPLOYMENT.md` §1), plus `jq`. It exits non-zero on any failure.

`make check` is the gate (fmt, clippy, tests, `make leakscan`); `make
release-draft` + `make release` and `make deploy-build` + `make deploy` cut and
deploy releases (see RELEASING.md). None of these touch the deployed server: it
runs a release from the data dir, not this checkout.

`make import`, `make serve` and release builds (`scripts/deploy.sh build`) use the `encryption` feature,
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
- **Oura tests** (`vitals-oura`) run the real sync against a scripted HTTP server on loopback: refresh first when expired, refresh once on a 401, a dead refresh token failing without touching the token file, paging, chunked backfill, and the auth `state` check. Core's `oura` tests cover parsing, upserts, the reconcile window and every metric mapping. Build with `--features oura` (`make check` does).
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
vitals daily --metrics oura_readiness,oura_sleep_total_min,oura_hrv_avg
```

Add `--json` to any of them, or `--server URL` with the client build.
Global flags go before or after the subcommand.

### JSON output and exit codes

The CLI follows the conventions agent wrappers share, so a caller can branch
on the exit code alone. With `--json` (or `--server`, always JSON) it prints
exactly one JSON object on stdout:

- **Success:** `{"ok":true, ...}`. An object payload's fields sit at the top
  level beside `ok`, in the API's serde shape. A list goes under `items`
  (`vitals sleep --json` → `{"ok":true,"items":[...]}`).
- **Failure:** `{"ok":false,"error":{"code":N,"message":"..."}}`, where
  `code` is the exit code.

Without `--json`, output is text and errors go to stderr as `error: ...`.
`route --gpx` prints GPX either way. The HTTP API itself stays bare.

| Exit | Meaning | e.g. |
|---|---|---|
| 0 | done | |
| 1 | internal: a bug, or an error it can't classify | DB error |
| 2 | usage: bad arguments, or input it won't take | bad date, unknown flag, `import` with `--server` |
| 3 | not found | `workout 99`, `import` of a missing file |
| 4 | conflict | (no command yet) |
| 5 | unreachable: a service is down, slow or rate-limited | server unreachable, HTTP 502/503/504/429, an Oura sync failure |
| 6 | denied: credentials refused or unreadable | HTTP 401, unreadable `VITALS_DB_KEY_FILE`, Oura not configured |
| 7 | refused: a guard said no | HTTP 403: the read-only token on a raw route |
`vitals backup --to PATH` writes a verified snapshot of the DB (local only;
see DEPLOYMENT §7).
