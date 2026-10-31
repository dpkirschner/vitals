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
trap 'rm -rf "$TMP"' EXIT

python3 -I scripts/synth-export.py --out "$TMP" --days "$DAYS" --seed "$SEED" ${END:+--end "$END"}

echo "building release CLI…"
cargo build -q --release --features encryption -p vitals-cli
rm -f "$DB" "$DB-wal" "$DB-shm"
target/release/vitals --db "$DB" import "$TMP/apple_health_export/export.xml"
target/release/vitals --db "$DB" status
