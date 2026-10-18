# vitals

A local, encrypted archive of your Apple Health data, with a CLI and a
read-only JSON API for you and your agents. It is a sibling of
[outflow](https://github.com/dpkirschner/outflow) and follows the same
conventions: a Rust workspace (`core` / `cli` / `server`), SQLite with
SQLCipher, secrets taken from env vars or 0600 files, a loopback server
behind `tailscale serve`, and a two-tier bearer token.

```
iPhone Health app ── Export All Health Data ──► export.zip
                                                   │  vitals import
                                                   ▼
                                     encrypted SQLite (records, workouts,
                                     routes, activity rings, daily rollups)
                                         │                     │
                               vitals CLI (local)      vitals-server /api/*
                                                               │  tailscale serve
                                                  agents: vitals --server … --json
```

## Quick start (dev)

```
make check
make import EXPORT=~/Downloads/export.zip   # into ~/vitals-dev.db
cargo run -p vitals-cli -- --db ~/vitals-dev.db daily --since 2026-10-01
```

Encrypted builds read the key from `VITALS_DB_KEY_FILE`, as in outflow.

## Docs

| Doc | What's in it |
|---|---|
| [ARCHITECTURE](docs/ARCHITECTURE.md) | crates, data flow, why it's shaped this way |
| [DATA_MODEL](docs/DATA_MODEL.md) | tables, keys, rollup metrics |
| [INVARIANTS](docs/INVARIANTS.md) | rules the code must keep |
| [GOTCHAS](docs/GOTCHAS.md) | Apple Health export quirks |
| [DEVELOPMENT](docs/DEVELOPMENT.md) | build, test, fixtures |
| [DEPLOYMENT](docs/DEPLOYMENT.md) | launchd daemon, tokens, tailscale, agent access |
| [API](docs/API.md) | every endpoint and its auth tier |

## Your data stays yours

The repo contains code and **synthetic** fixtures only. Your export, the
database, and every key and token live outside the repo (in
`~/Library/Application Support/vitals/` by default) or are gitignored
(`*.zip`, `export*.xml`, `*.gpx`, `*.db`). Nothing leaves your machine:
vitals makes no network calls of its own.

## License

[PolyForm Noncommercial 1.0.0](LICENSE).
