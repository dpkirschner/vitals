# Deployment (always-on Mac, launchd, Tailscale)

Same shape as outflow's: a LaunchDaemon runs `vitals-server` on loopback,
`tailscale serve` adds HTTPS on the tailnet, and secrets live in 0600 files.

## 1. Data directory and secrets

```
mkdir -p ~/Library/"Application Support"/vitals && chmod 700 ~/Library/"Application Support"/vitals
```

```
cd ~/Library/"Application Support"/vitals && umask 077 && openssl rand -hex 32 > db-key && openssl rand -hex 32 > api-token && openssl rand -hex 32 > api-token-ro
```

**The DB key is the one secret you cannot lose.** A lost `db-key` orphans the
encrypted archive for good. Put a copy in a password manager. The API tokens can be
regenerated with `openssl rand` at any time. Losing the archive itself is
recoverable, since you can import a fresh export, but the key is still what protects it.

## 2. Build and import

This section and §3 are first-time setup. From then on every deploy is a
release (see "Deploying a release" below), and the first install moves the
daemon off this checkout.

The daemon's account needs its own clone, writable by nobody else (outflow
DEPLOYMENT §5 explains why).

```
cargo build --release --features encryption -p vitals-server
```

```
cargo build --release --features encryption,client -p vitals-cli
```

```
VITALS_DB_KEY_FILE=~/Library/"Application Support"/vitals/db-key ./target/release/vitals --db ~/Library/"Application Support"/vitals/vitals.db import ~/path/to/export.zip
```

Then delete `export.zip` or move it somewhere private. It is an unencrypted copy
of everything.

## 3. The plist

`/Library/LaunchDaemons/com.vitals.server.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.vitals.server</string>
  <key>UserName</key><string>YOU</string>
  <key>GroupName</key><string>staff</string>
  <key>ProgramArguments</key><array>
    <string>/Users/YOU/code/vitals/target/release/vitals-server</string>
  </array>
  <key>WorkingDirectory</key><string>/Users/YOU/code/vitals</string>
  <key>EnvironmentVariables</key><dict>
    <!-- Set explicitly: a daemon gets no HOME. -->
    <key>VITALS_DATA_DIR</key><string>/Users/YOU/Library/Application Support/vitals</string>
    <key>VITALS_LISTEN</key><string>127.0.0.1:8090</string>
    <!-- Secrets by FILE, never inline: this plist is world-readable. -->
    <key>VITALS_DB_KEY_FILE</key><string>/Users/YOU/Library/Application Support/vitals/db-key</string>
    <key>VITALS_API_TOKEN_FILE</key><string>/Users/YOU/Library/Application Support/vitals/api-token</string>
    <key>VITALS_API_TOKEN_RO_FILE</key><string>/Users/YOU/Library/Application Support/vitals/api-token-ro</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Users/YOU/Library/Logs/vitals-server.log</string>
  <key>StandardErrorPath</key><string>/Users/YOU/Library/Logs/vitals-server.log</string>
</dict></plist>
```

```
sudo cp com.vitals.server.plist /Library/LaunchDaemons/
```

```
sudo chown root:wheel /Library/LaunchDaemons/com.vitals.server.plist && sudo chmod 644 /Library/LaunchDaemons/com.vitals.server.plist
```

```
sudo launchctl bootstrap system /Library/LaunchDaemons/com.vitals.server.plist
```

### Deploying a release

Every deploy is a release ([RELEASING.md](RELEASING.md)). Each one lives in
its own folder, `<data dir>/releases/<tag>/` (`vitals-server`, `vitals`,
`BUILD`), and launchd runs `releases/current`, a symlink. In a terminal (sudo
asks for a password):

```
scripts/deploy.sh build vYYYY.MM.N     # no sudo; nothing live changes
scripts/deploy.sh install vYYYY.MM.N   # or: make deploy TAG=vYYYY.MM.N
```

`build` checks the tag is a published release on `origin/main`, checks it out
in a separate worktree (`~/vitals-build`), and compiles the server with
`encryption` and the CLI with `encryption,client`, both with
`VITALS_VERSION=<tag>`. `install` stops the daemon, copies the DB to
`vitals.db.pre-<tag>`, points `releases/current` at the tag, starts it, and
checks the main API endpoints (with the read-only token) and `GET
/api/version`, which must report the tag. `--check SCRIPT` adds a
release-specific check: `python3 SCRIPT DIR` runs on the fetched JSON. It then
keeps the newest five releases and three DB backups (the live release and the
rollback target are always kept). A plist that still points at a checkout's
`target/release` is moved to `releases/current` by the first install: the old
binaries are kept as `releases/pre-releases` and the old plist next to the DB
as `com.vitals.server.plist.pre-releases`.

```
scripts/deploy.sh status               # live tag, installed releases, history
scripts/deploy.sh rollback             # back to the previous release
scripts/deploy.sh rollback vX --restore-db   # also restore the DB backup
                                       # taken before the live release
```

Use `--restore-db` when rolling back past a release that bumped the schema:
an older build may not open a newer DB. The CLI that matches the live server
is `<data dir>/releases/current/vitals`.

A plain restart (no new release) is still
`sudo launchctl kickstart -k system/com.vitals.server`.

After a deploy, `make smoke DB=~/Library/"Application Support"/vitals/vitals.db`
runs the full API check against the production database on a spare port.

A CLI import can run while the daemon is serving. The DB is in WAL mode, and the
server picks up new rows on its next query, with no restart needed.

## 4. Tailscale

vitals uses its own HTTPS port, so it can share a tailnet name with another
service on 443 (for example outflow):

```
tailscale serve --bg --https=8443 http://127.0.0.1:8090
```

The URL is then `https://MINI.TAILNET.ts.net:8443`.

## 5. API tokens

| Token | Grants |
|---|---|
| `VITALS_API_TOKEN[_FILE]` | All of `/api/*`, including raw GPS routes. |
| `VITALS_API_TOKEN_RO[_FILE]` | `GET /api/*` except `/workouts/{id}/route` (403). |

Setting **either** token switches all of `/api` to deny-by-default (401 without a
valid token). With neither set, the API is open to anything that can reach the
port, including containers on this box via `host.docker.internal`. The server
logs a warning at startup when that's the case.

## 6. Agent / CLI access

Give agents the **read-only** token, never the full one:

```
export VITALS_SERVER=https://MINI.TAILNET.ts.net:8443
export VITALS_API_TOKEN=<the api-token-ro value>
vitals daily --since 2026-09-01 --json
```

The read-only token goes in the same `VITALS_API_TOKEN` variable. The CLI
forwards whatever bearer token it's given, and the server decides the tier.
`route` comes back 403 with a message saying raw GPS needs the full token.

**From a container on the mini**, MagicDNS usually doesn't resolve, so use the
host's loopback, which colima and Docker Desktop forward:

```
export VITALS_SERVER=http://host.docker.internal:8090
```

That route still requires the token. Being on the box is not authorization.

Every read subcommand (`status`, `types`, `records`, `series`, `daily`,
`sleep`, `activity`, `workouts`, `workout`, `route`) works in server mode and
prints the same JSON as local `--json`. `import` and `backup` are local only.

## 7. Backups

Don't copy the live `vitals.db`: it runs in WAL mode, so recent writes sit in
`vitals.db-wal`, and a plain file copy can catch it mid-write. Snapshot it with
the CLI and back up the snapshot. It is safe while the server runs:

```
VITALS_DB_KEY_FILE="$HOME/Library/Application Support/vitals/db-key" vitals --db "$HOME/Library/Application Support/vitals/vitals.db" backup --to "$HOME/vitals-backups/vitals-$(date +%F).db"
```

`backup` copies one consistent snapshot with SQLite's online backup API (WAL
included), keeps it encrypted with the same key, writes it as one 0600 file,
and refuses to overwrite. It checks the copy (integrity check, schema version,
row count per table) before the file appears under its name, so a file at
`--to` is always a verified backup. It never migrates the live DB. `--json`
gives the per-table counts. The CLI must be built with `--features
encryption` (as release builds are) for an encrypted DB.

To restore, stop the daemon, put the snapshot in place as `vitals.db` (remove
any old `vitals.db-wal` and `vitals.db-shm`), and start it again.
