#!/bin/bash
# deploy.sh: build and install vitals releases on a macOS launchd host.
#
#   scripts/deploy.sh build <tag> [--rebuild]
#   scripts/deploy.sh install <tag> [--check SCRIPT]
#   scripts/deploy.sh rollback [<tag>] [--restore-db]
#   scripts/deploy.sh status
#
# Every deploy is a release (docs/RELEASING.md). Each release lives in
# its own folder under <data dir>/releases/<tag>/ (vitals-server,
# vitals, BUILD) and launchd runs releases/current, a symlink.
# Installing or rolling back switches that link, so the development
# checkout is never what's live.
#
# build    No sudo. Checks the tag is a published release on origin/main,
#          checks it out in a separate build worktree, compiles with
#          VITALS_VERSION=<tag> (so /api/version reports it), and stages
#          releases/<tag>/. Nothing live changes.
# install  Needs sudo, so run it in a terminal. Stops the server, backs
#          up the DB as <db>.pre-<tag>, points current at <tag>, starts,
#          then checks the main API endpoints (with the read-only token
#          when one is configured) and /api/version. --check runs
#          `python3 SCRIPT DIR` on the fetched JSON for release-specific
#          checks. The first install moves launchd from the old layout
#          (binaries in a checkout's target/release) to releases/current:
#          the old binaries are kept as releases/pre-releases and the old
#          plist as <data dir>/<plist name>.pre-releases.
#          Afterwards it prunes to the newest $KEEP_RELEASES releases and
#          $KEEP_DB_BACKUPS DB backups.
# rollback Points current back at <tag>, default the release before the
#          live one. --restore-db also puts back the DB backup taken
#          before the live release was installed (needed after a
#          migration).
# status   What's live, what's installed, and the install history.
#
# Settings come from the launchd plist (data dir, listen address, token
# file, log); override with env: VITALS_PLIST, VITALS_BUILD_DIR
# (default ~/vitals-build), VITALS_FEATURES (default encryption,oura;
# the CLI also gets client), KEEP_RELEASES (5), KEEP_DB_BACKUPS (3). Tests
# set LAUNCHCTL and SUDO.
set -euo pipefail

PLIST=${VITALS_PLIST:-/Library/LaunchDaemons/com.vitals.server.plist}
BUILD_DIR=${VITALS_BUILD_DIR:-$HOME/vitals-build}
FEATURES=${VITALS_FEATURES-encryption,oura}
KEEP_RELEASES=${KEEP_RELEASES:-5}
KEEP_DB_BACKUPS=${KEEP_DB_BACKUPS:-3}
LAUNCHCTL=${LAUNCHCTL:-launchctl}
SUDO=${SUDO-sudo}
REPO=$(cd "$(dirname "$0")/.." && pwd)
TAG_RE='^v[0-9]{4}\.[0-9]{2}\.[0-9]+$'

STOPPED=0 SWITCHED=0
die() {
  echo "deploy: $*" >&2
  if [ "$STOPPED" = 1 ] || [ "$SWITCHED" = 1 ]; then
    [ "$STOPPED" = 0 ] || echo "deploy: the server may be down." >&2
    echo "deploy: restore the previous release with: scripts/deploy.sh rollback" >&2
  fi
  exit 1
}
step() { printf '\n--- %s\n' "$*"; }
pb() { /usr/libexec/PlistBuddy -c "Print :$1" "$PLIST" 2>/dev/null || true; }

load_plist() {
  [ -r "$PLIST" ] || die "can't read $PLIST"
  LABEL=$(pb Label)
  PROG=$(pb ProgramArguments:0)
  DATA=$(pb EnvironmentVariables:VITALS_DATA_DIR)
  DATA=${DATA:-$HOME/Library/Application Support/vitals}
  DB=$(pb EnvironmentVariables:VITALS_DB)
  DB=${DB:-$DATA/vitals.db}
  LISTEN=$(pb EnvironmentVariables:VITALS_LISTEN)
  LISTEN=${LISTEN:-127.0.0.1:8090}
  RO_FILE=$(pb EnvironmentVariables:VITALS_API_TOKEN_RO_FILE)
  LOG=$(pb StandardOutPath)
  [ -n "$LABEL" ] && [ -n "$PROG" ] || die "$PLIST has no Label or ProgramArguments"
  REL="$DATA/releases"
  CUR="$REL/current"
}

check_tag() {
  [[ $1 =~ $TAG_RE ]] || die "$1 isn't a release tag (vYYYY.MM.N)"
}

live_tag() { # the tag current points at, or empty
  [ -L "$CUR" ] && basename "$(readlink "$CUR")" || true
}

previous_tag() { # newest history entry that isn't live and still exists
  local live t
  live=$(live_tag)
  [ -f "$REL/history" ] || return 0
  # history lines: date time tag [note]
  tail -r "$REL/history" | awk '{print $3}' | while read -r t; do
    if [ "$t" != "$live" ] && [ -d "$REL/$t" ]; then echo "$t"; break; fi
  done
}

# ── build ──────────────────────────────────────────────────────────────
cmd_build() {
  local tag=$1 rebuild=${2:-}
  check_tag "$tag"
  load_plist
  cd "$REPO"
  git fetch --quiet --tags origin
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "no tag $tag"
  git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null ||
    die "$tag isn't on origin; publish the release first"
  git merge-base --is-ancestor "$tag" origin/main || die "$tag isn't on main"
  local dest="$REL/$tag"
  if [ -e "$dest" ]; then
    [ "$(live_tag)" != "$tag" ] || die "$tag is live; nothing to build"
    [ "$rebuild" = --rebuild ] || die "$dest exists; pass --rebuild to replace it"
    rm -rf "$dest"
  fi

  local src="$BUILD_DIR/src"
  step "check out $tag in $src"
  mkdir -p "$BUILD_DIR"
  if [ -d "$src/.git" ] || [ -f "$src/.git" ]; then
    git -C "$src" checkout --quiet --detach "$tag"
    git -C "$src" clean -fdq
  else
    git worktree add --quiet --detach "$src" "$tag"
  fi
  local sha
  sha=$(git -C "$src" rev-parse HEAD)

  step "compile (features: ${FEATURES:-none}, VITALS_VERSION=$tag)"
  local feat=() cli_feat=client
  [ -z "$FEATURES" ] || feat=(--features "$FEATURES")
  [ -z "$FEATURES" ] || cli_feat="$FEATURES,client"
  (cd "$src" && export VITALS_VERSION="$tag" CARGO_TARGET_DIR="$BUILD_DIR/target" &&
    cargo build --release --locked ${feat[@]+"${feat[@]}"} -p vitals-server &&
    cargo build --release --locked --features "$cli_feat" -p vitals-cli)

  step "stage $dest"
  local part="$REL/.$tag.partial"
  rm -rf "$part"
  mkdir -p "$part"
  cp -p "$BUILD_DIR/target/release/vitals-server" "$BUILD_DIR/target/release/vitals" "$part/"
  printf 'tag %s\ncommit %s\nbuilt %s\nfeatures %s\n' \
    "$tag" "$sha" "$(date '+%F %T %z')" "${FEATURES:-none}" >"$part/BUILD"
  mv "$part" "$dest"
  local v
  if v=$("$dest/vitals" --version 2>/dev/null); then
    [ "$v" = "vitals $tag" ] || die "built CLI reports '$v', expected vitals $tag"
  else
    v=
  fi
  echo "built $tag ($sha): $dest"
  [ -n "$v" ] || echo "note: $tag predates version reporting (vitals --version)"
  echo "next, in a terminal: scripts/deploy.sh install $tag"
}

# ── shared: stop, start, checks ────────────────────────────────────────
stop_server() {
  STOPPED=1
  $SUDO "$LAUNCHCTL" bootout "system/$LABEL" || die "bootout failed"
  local _
  for _ in $(seq 1 30); do
    pgrep -f "$PROG" >/dev/null || break
    sleep 1
  done
  ! pgrep -f "$PROG" >/dev/null || die "server still running after bootout"
  echo "stopped"
}

start_server() {
  local before=0 up=0 _
  [ -z "$LOG" ] || before=$(wc -l <"$LOG" 2>/dev/null || echo 0)
  $SUDO "$LAUNCHCTL" bootstrap system "$PLIST" || die "bootstrap failed"
  if [ -n "$LOG" ]; then
    for _ in $(seq 1 60); do
      if tail -n +"$((before + 1))" "$LOG" 2>/dev/null | grep -q "vitals server up"; then
        up=1
        break
      fi
      sleep 1
    done
    echo "new log lines:"
    tail -n +"$((before + 1))" "$LOG" | tail -15 | sed 's/^/   /'
    [ "$up" = 1 ] || die "no 'vitals server up' in $LOG within 60s"
  fi
  STOPPED=0
}

api_get() { # name path -> $TMP/name.json, prints the status code
  if [ -n "$RO_FILE" ] && [ -r "$RO_FILE" ]; then
    # The token goes to curl on stdin, never argv.
    printf 'Authorization: Bearer %s\n' "$(cat "$RO_FILE")" |
      curl -s -H @- -o "$TMP/$1.json" -w '%{http_code}' "http://$LISTEN/api$2"
  else
    curl -s -o "$TMP/$1.json" -w '%{http_code}' "http://$LISTEN/api$2"
  fi
}

run_checks() { # tag [check-script]
  local tag=$1 extra=${2:-} path code name
  step "checks"
  TMP=$(mktemp -d "${TMPDIR:-/tmp}/vitals-deploy.XXXXXX")
  while read -r name path; do
    code=$(api_get "$name" "$path")
    echo "GET /api$path -> $code"
    [ "$code" = 200 ] || die "/api$path returned $code"
  done <<'EOF'
status /status
types /types
metrics /metrics
records /records?limit=5
series /series?type=StepCount&bucket=month
daily /daily
sleep /sleep
activity /activity
workouts /workouts?limit=5
EOF
  code=$(api_get version /version)
  if [ "$code" = 200 ]; then
    local got
    got=$(/usr/bin/python3 -c '
import json, sys
try:
    print(json.load(open(sys.argv[1])).get("version"))
except Exception:
    print("unreadable")' "$TMP/version.json")
    echo "GET /api/version -> $got"
    [ "$tag" = pre-releases ] || [ "$got" = "$tag" ] || die "server reports $got, expected $tag"
  else
    echo "GET /api/version -> $code (this release predates /api/version)"
  fi
  if [ -n "$extra" ]; then
    /usr/bin/python3 -I "$extra" "$TMP" || die "$extra failed"
  fi
  rm -rf "$TMP"
}

# ── install ────────────────────────────────────────────────────────────
migrate_plist() { # old layout -> releases/current; server must be stopped
  local legacy="$REL/pre-releases" new
  step "move launchd to $CUR"
  if [ ! -d "$legacy" ]; then
    mkdir -p "$legacy.partial"
    cp -p "$PROG" "$legacy.partial/vitals-server"
    [ ! -e "$(dirname "$PROG")/vitals" ] || cp -p "$(dirname "$PROG")/vitals" "$legacy.partial/"
    printf 'tag pre-releases\nfrom %s\nsaved %s\n' "$PROG" "$(date '+%F %T %z')" \
      >"$legacy.partial/BUILD"
    mv "$legacy.partial" "$legacy"
    echo "kept the old build as $legacy"
  fi
  cp -p "$PLIST" "$DATA/$(basename "$PLIST").pre-releases"
  new=$(mktemp "${TMPDIR:-/tmp}/vitals-plist.XXXXXX")
  cp "$PLIST" "$new"
  /usr/libexec/PlistBuddy -c "Set :ProgramArguments:0 $CUR/vitals-server" "$new"
  /usr/libexec/PlistBuddy -c "Delete :WorkingDirectory" "$new" 2>/dev/null || true
  /usr/libexec/PlistBuddy -c "Add :WorkingDirectory string $DATA" "$new"
  plutil -lint -s "$new" || die "rewritten plist doesn't lint"
  if [ -n "$SUDO" ]; then
    $SUDO install -m 644 -o root -g wheel "$new" "$PLIST"
  else
    install -m 644 "$new" "$PLIST"
  fi
  rm -f "$new"
  # pre-releases is the rollback target until the first install succeeds.
  ln -sfn pre-releases "$CUR"
  echo "$(date '+%F %T') pre-releases" >>"$REL/history"
  echo "launchd now runs $CUR/vitals-server (old plist kept in $DATA)"
}

prune() {
  local live prev keep d n
  live=$(live_tag)
  prev=$(previous_tag)
  n=0
  # Newest first by when they were installed or built.
  for d in $(ls -t "$REL" | grep -E "$TAG_RE|^pre-releases$"); do
    if [ "$d" = "$live" ] || [ "$d" = "$prev" ]; then continue; fi
    n=$((n + 1))
    keep=$((KEEP_RELEASES - 2))
    if [ "$n" -gt "$keep" ]; then
      rm -rf "${REL:?}/$d"
      echo "pruned release $d"
    fi
  done
  n=0
  # Only backups this script made (pre-<release tag>); older ones such as
  # pre-v15 (a schema version) are left alone.
  for d in $(ls -t "$(dirname "$DB")" | grep -E "^$(basename "$DB")\.pre-v[0-9]{4}\.[0-9]{2}\.[0-9]+$"); do
    n=$((n + 1))
    if [ "$n" -gt "$KEEP_DB_BACKUPS" ]; then
      rm -f "$(dirname "$DB")/$d" "$(dirname "$DB")/$d-wal" "$(dirname "$DB")/$d-shm"
      echo "pruned DB backup $d"
    fi
  done
}

cmd_install() {
  local tag=$1 extra=${2:-}
  check_tag "$tag"
  [ -t 0 ] || [ -z "$SUDO" ] || die "run install in a terminal (sudo asks for a password)"
  load_plist
  local dest="$REL/$tag"
  [ -f "$dest/BUILD" ] || die "$tag isn't built; run: scripts/deploy.sh build $tag"
  [ "$(live_tag)" != "$tag" ] || die "$tag is already live"
  [ -z "$extra" ] || [ -r "$extra" ] || die "can't read $extra"
  [ -e "$DB" ] || die "no DB at $DB"
  [ ! -e "$DB.pre-$tag" ] ||
    die "$tag was installed before ($DB.pre-$tag exists); go back to it with: scripts/deploy.sh rollback $tag"
  local migrate=0
  [ "$PROG" = "$CUR/vitals-server" ] || migrate=1
  step "install $tag (live: $(live_tag || true))"
  [ "$migrate" = 0 ] || echo "launchd runs $PROG today; this install moves it to $CUR"
  sed 's/^/   /' "$dest/BUILD"
  [ -z "$SUDO" ] || $SUDO -v

  step "stop"
  stop_server
  step "back up the DB to $DB.pre-$tag"
  cp -p "$DB" "$DB.pre-$tag"
  local ext
  for ext in -wal -shm; do
    [ ! -e "$DB$ext" ] || cp -p "$DB$ext" "$DB.pre-$tag$ext"
  done
  cmp -s "$DB" "$DB.pre-$tag" || die "DB backup differs from the original"
  [ "$migrate" = 0 ] || migrate_plist
  load_plist # PROG may have changed
  step "switch current to $tag"
  ln -sfn "$tag" "$CUR"
  SWITCHED=1
  echo "$(date '+%F %T') $tag" >>"$REL/history"
  step "start"
  start_server
  run_checks "$tag" "$extra"
  step "prune"
  prune
  printf '\n=== %s is live ===\n' "$tag"
}

# ── rollback ───────────────────────────────────────────────────────────
cmd_rollback() {
  local tag=${1:-} restore=${2:-}
  load_plist
  [ "$PROG" = "$CUR/vitals-server" ] || die "launchd isn't on releases/current yet; nothing to roll back to"
  [ -t 0 ] || [ -z "$SUDO" ] || die "run rollback in a terminal (sudo asks for a password)"
  local live
  live=$(live_tag)
  [ -n "$tag" ] || tag=$(previous_tag)
  [ -n "$tag" ] || die "no earlier release to roll back to"
  [ -d "$REL/$tag" ] || die "$tag isn't installed in $REL"
  [ "$tag" != "$live" ] || die "$tag is already live"
  if [ "$restore" = --restore-db ]; then
    [ -e "$DB.pre-$live" ] || die "no $DB.pre-$live to restore"
  fi
  step "roll back $live -> $tag${restore:+ (restoring the DB from before $live)}"
  [ -z "$SUDO" ] || $SUDO -v
  stop_server
  if [ "$restore" = --restore-db ]; then
    cp -p "$DB" "$DB.rolled-back-$live"
    cp -p "$DB.pre-$live" "$DB"
    rm -f "$DB-wal" "$DB-shm"
    local ext
    for ext in -wal -shm; do
      [ ! -e "$DB.pre-$live$ext" ] || cp -p "$DB.pre-$live$ext" "$DB$ext"
    done
    echo "DB restored (the replaced one is $DB.rolled-back-$live)"
  fi
  ln -sfn "$tag" "$CUR"
  echo "$(date '+%F %T') $tag rollback from $live" >>"$REL/history"
  start_server
  run_checks "$tag"
  printf '\n=== rolled back to %s ===\n' "$tag"
}

# ── status ─────────────────────────────────────────────────────────────
cmd_status() {
  load_plist
  if [ "$PROG" != "$CUR/vitals-server" ]; then
    echo "launchd runs $PROG (old layout; the first install moves it to releases/)"
  else
    echo "live: $(live_tag)"
    [ ! -f "$CUR/BUILD" ] || sed 's/^/   /' "$CUR/BUILD"
  fi
  TMP=$(mktemp -d "${TMPDIR:-/tmp}/vitals-deploy.XXXXXX")
  local code
  code=$(api_get version /version || true)
  if [ "$code" = 200 ]; then
    echo "server says: $(cat "$TMP/version.json")"
  else
    echo "server says: /api/version -> ${code:-no answer}"
  fi
  rm -rf "$TMP"
  if [ -d "$REL" ]; then
    echo "installed: $(ls -t "$REL" | grep -E "$TAG_RE|^pre-releases$" | tr '\n' ' ')"
    echo "rollback target: $(previous_tag)"
    [ ! -f "$REL/history" ] || { echo "history:"; tail -5 "$REL/history" | sed 's/^/   /'; }
  fi
}

case ${1:-} in
build)
  [ $# -ge 2 ] || die "usage: deploy.sh build <tag> [--rebuild]"
  cmd_build "$2" "${3:-}"
  ;;
install)
  [ $# -eq 2 ] || { [ $# -eq 4 ] && [ "$3" = --check ]; } ||
    die "usage: deploy.sh install <tag> [--check SCRIPT]"
  cmd_install "$2" "${4:-}"
  ;;
rollback)
  shift
  t= r=
  for a in "$@"; do
    case $a in --restore-db) r=$a ;; *) t=$a ;; esac
  done
  cmd_rollback "$t" "$r"
  ;;
status) cmd_status ;;
*)
  sed -n '2,7p' "$0" >&2
  exit 2
  ;;
esac
