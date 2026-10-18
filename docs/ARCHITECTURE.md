# Architecture

## Crates

| Crate | Role |
|---|---|
| `core` (`vitals-core`) | The pure domain. It has no network or GUI code. Modules: `health_export` (streaming XML/GPX parser), `import` (idempotent ingest), `store` (schema and migrations), `rollup` (daily metrics), `query` (every read, shared by the CLI and the server), `time`, `units`, `geo`, `secrets`. |
| `cli` (`vitals`) | Local mode opens the DB directly. With the `client` feature, `--server` turns each read into a single GET and prints the server's JSON verbatim. |
| `server` (`vitals-server`) | axum 0.8. It serves `GET /api/*` only, with bearer auth in two tiers. There is no web UI. |

Feature flags: `encryption` (SQLCipher) on every crate, and `client` (ureq) on
the CLI.

## Data flow

1. **Export.** On the iPhone: Health app → profile → *Export All Health Data*. This
   produces `export.zip`, which contains `apple_health_export/export.xml` (over 1 GB),
   `workout-routes/*.gpx` and `export_cda.xml`. vitals ignores `export_cda.xml` because
   it duplicates `export.xml`.
2. **Import.** `vitals import export.zip` streams `export.xml` out of the zip with
   `quick-xml`. Nothing is extracted to disk and no DOM is built. Each closed
   `<Record>`, `<Workout>` or `<ActivitySummary>` becomes a domain struct and is
   written with `INSERT OR IGNORE` on its natural key, in transactions of
   about 50k rows. GPX routes are read after the XML pass, because the zip can only
   stream one entry at a time.
3. **Rollup.** The `daily` table is recomputed in Rust for the span of local days
   the new rows touched, plus or minus one day.
4. **Read.** `core::query` is the only read path. The CLI's local `--json` output
   and the API return the same structs, so the JSON is byte-for-byte identical.

A multi-year Apple Watch export (millions of records, about 1 GB of XML) imports in
well under a minute on an Apple Silicon Mac, into a database of a few hundred MB.
Re-importing the same export adds nothing.

## Why no MCP or SQL endpoint

The same reasoning as outflow: agents use the CLI in `--server` mode with the
read-only token. Their reach is then a fixed, documented set of queries
(see API.md), and the method-based token tier stays meaningful.
