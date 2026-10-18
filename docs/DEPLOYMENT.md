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

To restart after a rebuild, run `make prod`, or:

```
sudo launchctl kickstart -k system/com.vitals.server
```

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
prints the same JSON as local `--json`. `import` is local only.
