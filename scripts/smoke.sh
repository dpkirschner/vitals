#!/usr/bin/env bash
# Smoke-test vitals-server end to end against a real (encrypted) database:
# auth tiers, every route, error shapes, and CLI --server output == local --json.
#
# Starts its own server on a spare port and stops it on exit; never touches a
# running daemon. Token values are read from files and never printed.
#
#   make smoke                       # defaults below
#   make smoke DB=/path/to/vitals.db PORT=8099
#
# Env: VITALS_DATA_DIR (holds db-key, api-token, api-token-ro), DB, PORT.
set -euo pipefail

DATA_DIR=${VITALS_DATA_DIR:-"$HOME/Library/Application Support/vitals"}
DB=${DB:-"$HOME/vitals-dev.db"}
PORT=${PORT:-8091}
BIN=target/release
U="http://127.0.0.1:$PORT/api"

for f in db-key api-token api-token-ro; do
  [[ -r "$DATA_DIR/$f" ]] || { echo "missing $DATA_DIR/$f (see docs/DEPLOYMENT.md §1)" >&2; exit 2; }
done
[[ -f "$DB" ]] || { echo "no database at $DB — run \`make import\` first" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }

echo "building release binaries…"
cargo build -q --release --features encryption -p vitals-server
cargo build -q --release --features encryption,client -p vitals-cli

RO=$(cat "$DATA_DIR/api-token-ro")
FULL=$(cat "$DATA_DIR/api-token")
export VITALS_DB_KEY_FILE="$DATA_DIR/db-key"

TMP=$(mktemp -d)
cleanup() {
  local rc=$?
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
  exit "$rc"
}
trap cleanup EXIT

VITALS_DB="$DB" VITALS_LISTEN="127.0.0.1:$PORT" \
  VITALS_API_TOKEN_FILE="$DATA_DIR/api-token" VITALS_API_TOKEN_RO_FILE="$DATA_DIR/api-token-ro" \
  "$BIN/vitals-server" >"$TMP/server.log" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 50); do
  curl -s -o /dev/null "$U/status" && break
  kill -0 "$SERVER_PID" 2>/dev/null || { echo "server exited:" >&2; cat "$TMP/server.log" >&2; exit 1; }
  sleep 0.2
done

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m    %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf '  \033[31mFAIL\033[0m  %s\n' "$1"; }

# expect CODE METHOD PATH [TOKEN] — TOKEN is a variable name, so values never hit argv logs.
expect() {
  local want=$1 method=$2 path=$3 tok=${4:-} hdr=()
  [[ -n "$tok" ]] && hdr=(-H "Authorization: Bearer ${!tok}")
  local got
  got=$(curl -s -o "$TMP/body" -w '%{http_code}' -X "$method" ${hdr[@]+"${hdr[@]}"} "$U$path")
  if [[ "$got" == "$want" ]]; then ok "$want  $method $path ${tok:+($tok)}"; else bad "$method $path ${tok:+($tok)}: got $got, want $want"; fi
}

# json PATH JQ-EXPR DESCRIPTION — GET with the RO token, assert jq -e on the body.
json() {
  if curl -s -H "Authorization: Bearer $RO" "$U$1" | jq -e "$2" >/dev/null 2>&1; then ok "$3"; else bad "$3 ($1)"; fi
}

echo "auth tiers"
BAD=not-a-real-token
expect 401 GET  /status
expect 401 GET  /status        BAD
expect 200 GET  /status        RO
expect 200 GET  /status        FULL
expect 403 POST /status        RO

# Pick real ids from the archive.
curl -s -H "Authorization: Bearer $RO" "$U/workouts?limit=1000" >"$TMP/workouts.json"
ANY_ID=$(jq -r '.[0].id // empty' "$TMP/workouts.json")
ROUTE_ID=$(jq -r '[.[] | select(.has_route)][0].id // empty' "$TMP/workouts.json")

echo "routes (read-only token)"
for p in /types /metrics "/records?type=StepCount&limit=5" "/series?type=StepCount&bucket=month" \
         /daily /sleep /activity "/workouts?limit=5"; do
  expect 200 GET "$p" RO
done
if [[ -n "$ANY_ID" ]]; then expect 200 GET "/workouts/$ANY_ID" RO; else bad "no workouts in archive"; fi

echo "GPS points are full-token only"
if [[ -n "$ROUTE_ID" ]]; then
  expect 403 GET "/workouts/$ROUTE_ID/route" RO
  expect 200 GET "/workouts/$ROUTE_ID/route" FULL
  if curl -s -H "Authorization: Bearer $FULL" "$U/workouts/$ROUTE_ID/route" | jq -e 'length > 0 and (.[0] | has("lat") and has("lon"))' >/dev/null; then
    ok "route has points with lat/lon"
  else
    bad "route $ROUTE_ID returned no points"
  fi
  json "/workouts/$ROUTE_ID" '(.routes | length > 0) and (.routes[0] | has("lat") | not)' "workout detail has route summary, no coordinates"
else
  echo "  skip  (no workout with a route)"
fi

echo "errors"
expect 404 GET /nope RO
expect 400 GET "/series?type=NotARealType" RO
expect 400 GET "/daily?metrics=bogus" RO
expect 404 GET /workouts/999999999 RO

echo "response shapes"
json /status '.records > 0 and .workouts >= 0 and (.imports | length > 0)' "status has records and imports"
json /types 'length > 0 and (.[0] | has("name") and has("count") and has("default_agg"))' "types listed with counts"
json "/records?type=HeartRate&limit=3" '.total > 0 and (.items | length == 3)' "records paginate"
json "/series?type=StepCount&bucket=month" '.agg == "sum" and (.points | length > 0) and all(.points[]; .value >= 0)' "monthly step series"
json "/daily?metrics=steps" 'length > 0 and all(.[]; .metrics | keys == ["steps"])' "daily filters metrics"
json /metrics 'index("sleep_asleep_min") != null' "metrics list"

echo "CLI --server output == local --json"
for args in "types" "status" "daily --since 2026-01-01" "sleep" "activity" "workouts --limit 20" \
            "series RestingHeartRate --bucket month"; do
  # shellcheck disable=SC2086
  remote=$(VITALS_API_TOKEN=$RO "$BIN/vitals" --server "http://127.0.0.1:$PORT" $args 2>&1 || true)
  # shellcheck disable=SC2086
  local_=$("$BIN/vitals" --db "$DB" --json $args 2>&1 || true)
  if [[ "$remote" == "$local_" ]]; then ok "vitals $args"; else bad "vitals $args differs between --server and local"; fi
done
if [[ -n "$ROUTE_ID" ]]; then
  # The CLI exits non-zero here by design; capture first so pipefail doesn't hide the match.
  out=$(VITALS_API_TOKEN=$RO "$BIN/vitals" --server "http://127.0.0.1:$PORT" route "$ROUTE_ID" 2>&1 || true)
  if [[ "$out" == *"HTTP 403: read-only token"* ]]; then
    ok "vitals route with RO token explains the 403"
  else
    bad "vitals route with RO token did not report read-only 403"
  fi
fi

echo
if [[ $FAIL -eq 0 ]]; then
  printf '\033[32m%d passed\033[0m\n' "$PASS"
else
  printf '\033[31m%d failed\033[0m, %d passed — server log: %s\n' "$FAIL" "$PASS" "$TMP/server.log"
  cp "$TMP/server.log" ./smoke-server.log 2>/dev/null && echo "(copied to ./smoke-server.log)"
  exit 1
fi
