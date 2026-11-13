#!/usr/bin/env bash
# Rebuild the dev database from synthetic data (scripts/synth-export.py), so
# `make smoke` and manual testing run against realistic, awkward data without
# any real health records. This is the integration-test setup.
#
#   make dev-db                      # replaces ~/vitals-dev.db
#   make dev-db DB=/tmp/other-dev.db DAYS=90 SEED=7
#
# The DB is deleted and recreated, so its file name must contain "dev". The
# generated export lives in a temp directory and is removed on exit.
#
# Oura data comes the real way: `vitals oura sync` against
# scripts/oura-mock.py (synthetic documents, fake tokens) on a loopback port.
#
# Env: VITALS_DATA_DIR (holds db-key), DB, DAYS, SEED, END (YYYY-MM-DD).
set -euo pipefail

DATA_DIR=${VITALS_DATA_DIR:-"$HOME/Library/Application Support/vitals"}
DB=${DB:-"$HOME/vitals-dev.db"}
DAYS=${DAYS:-240}
SEED=${SEED:-1}

case "$(basename "$DB")" in
  *dev*) ;;
  *) echo "refusing to replace $DB: dev-db only rebuilds databases named *dev*" >&2; exit 2 ;;
esac
[[ -r "$DATA_DIR/db-key" ]] || { echo "missing $DATA_DIR/db-key (see docs/DEPLOYMENT.md §1)" >&2; exit 2; }
export VITALS_DB_KEY_FILE="$DATA_DIR/db-key"

TMP=$(mktemp -d)
MOCK=
stop_mock() { [[ -z "$MOCK" ]] || { kill "$MOCK" 2>/dev/null; wait "$MOCK" 2>/dev/null; } || true; }
trap 'stop_mock; rm -rf "$TMP"' EXIT

python3 -I scripts/synth-export.py --out "$TMP" --days "$DAYS" --seed "$SEED" ${END:+--end "$END"}

echo "building release CLI…"
cargo build -q --release --features encryption,oura -p vitals-cli
rm -f "$DB" "$DB-wal" "$DB-shm"
target/release/vitals --db "$DB" import "$TMP/apple_health_export/export.xml"

echo "syncing synthetic Oura data from scripts/oura-mock.py…"
python3 -I scripts/oura-mock.py --days "$DAYS" --seed "$SEED" ${END:+--end "$END"} \
  --port-file "$TMP/oura-port" &
MOCK=$!
for _ in $(seq 1 50); do [[ -s "$TMP/oura-port" ]] && break; sleep 0.1; done
[[ -s "$TMP/oura-port" ]] || { echo "oura-mock didn't start" >&2; exit 1; }
(
  umask 077
  echo synthetic-client >"$TMP/oura-client-id"
  echo synthetic-secret >"$TMP/oura-client-secret"
  # Already expired, so the sync exercises a token refresh first.
  echo '{"access_token":"synthetic","refresh_token":"synthetic","expires_at":0}' \
    >"$TMP/oura-tokens.json"
)
OURA_CLIENT_ID_FILE="$TMP/oura-client-id" OURA_CLIENT_SECRET_FILE="$TMP/oura-client-secret" \
  OURA_REDIRECT_URI=https://vitals.example/oura/callback OURA_TOKENS_FILE="$TMP/oura-tokens.json" \
  OURA_API_BASE="http://127.0.0.1:$(cat "$TMP/oura-port")" \
  target/release/vitals --db "$DB" oura sync
target/release/vitals --db "$DB" status
