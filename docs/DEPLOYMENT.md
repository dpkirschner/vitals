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
cargo build --release --features encryption,oura -p vitals-server
```

```
cargo build --release --features encryption,client,oura -p vitals-cli
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

`install` and `rollback` also write `releases/<tag>/INSTALLED` (when it
started, and the log line it started at). `verify` reads it, needs no sudo, and
prints only what it observed:

```
scripts/deploy.sh verify                       # live tag, /api/version, checks
scripts/deploy.sh verify --log 'server up' \
  --wait-log 'oura sync' --timeout 900          # log lines since the install
```

`--log RE` (repeatable) looks for a line in the log written since the live
release started; `--wait-log RE` polls for one until `--timeout` seconds
(default 900), for things that happen later, such as the first Oura sync.
`--check SCRIPT` works as in `install`. Anything it doesn't find is printed
as `NOT FOUND`, and `verify` exits 1.

### Changing the plist's environment

The plist is root-owned, so edits go through `deploy.sh env` rather than a
hand-run PlistBuddy:

```
scripts/deploy.sh env list
scripts/deploy.sh env set VITALS_OURA_SYNC_INTERVAL_SECS=3600  # sudo
scripts/deploy.sh env unset VITALS_OURA_SYNC_INTERVAL_SECS
scripts/deploy.sh env restore            # back to the newest backup
```

`set`, `unset` and `restore` edit a temp copy, lint it, show the diff and
ask before installing it (root:wheel 644). The live plist is first copied to
`<data dir>/plist-backups/` (the newest ten are kept). Keys that look like
secrets (`*TOKEN`, `*SECRET`, `*PASSWORD`, `*KEY`) are refused: point the
`_FILE` variant at a 0600 file instead. launchd reads the environment when the
service starts, so a change takes effect at the next `install` or `rollback`,
or now with `--restart`. A rollback keeps the current environment; if the
older release can't run with it, `env restore` first.

A plain restart (no new release, no env change) is still
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
prints the same JSON as local `--json`. `import`, `backup` and `oura` are local only.

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

## 8. Oura ring (optional)

With Oura configured, the server pulls the ring's daily documents from the
Oura API v2: shortly after start, then every 6h. Without it, vitals makes no
network calls. Oura data shows up as `oura_*` metrics in `/daily` (DATA_MODEL).

**One-time setup:**

1. Register a personal app at `cloud.ouraring.com/oauth/applications`. Oura
   rejects `http://localhost` redirects, so register the tailnet URL of this
   server: `https://MINI.TAILNET.ts.net:8443/oura/callback`. The redirect
   happens in your browser, so the URL doesn't need to be public.
2. Save the app's client id and secret as 0600 files in the data dir,
   `oura-client-id` and `oura-client-secret`.
3. Add them to the plist (in a terminal; `--restart` applies them now, or
   leave it off when an `install` follows):

   ```
   D="$HOME/Library/Application Support/vitals"
   scripts/deploy.sh env set --restart \
     OURA_CLIENT_ID_FILE="$D/oura-client-id" \
     OURA_CLIENT_SECRET_FILE="$D/oura-client-secret" \
     OURA_REDIRECT_URI=https://MINI.TAILNET.ts.net:8443/oura/callback
   ```

4. Authorize once, as the user the daemon runs as, with the same settings:

   ```
   D="$HOME/Library/Application Support/vitals"
   OURA_CLIENT_ID_FILE="$D/oura-client-id" OURA_CLIENT_SECRET_FILE="$D/oura-client-secret" OURA_REDIRECT_URI=https://MINI.TAILNET.ts.net:8443/oura/callback VITALS_DB_KEY_FILE="$D/db-key" "$D/releases/current/vitals" --db "$D/vitals.db" oura auth
   ```

   It prints a URL. Open it, approve, and paste back the full address the
   browser lands on (the server's `/oura/callback` page says to). That writes
   `oura-tokens.json` (0600) next to the DB. Run the same command with
   `oura sync` instead of `oura auth` to backfill all history right away, or
   wait for the server's next run.

**Settings:** `OURA_TOKENS_FILE` (default `oura-tokens.json` next to the DB),
`VITALS_OURA_SYNC_INTERVAL_SECS` (default 21600, minimum 300). A set but
unreadable `OURA_*_FILE`, or a partial config, stops the server at startup.

**When it breaks:** `vitals status` (or `/api/status`) lists recent Oura syncs
with their errors. A refresh failure (`invalid_grant`) means the connection is
gone: run `oura auth` again. HTTP 403 means the Oura membership lapsed or a
scope is missing. Each sync re-fetches the last 7 days, so a missed run fills
itself in; `oura sync --since YYYY-MM-DD` re-fetches further back.

