#!/usr/bin/env bash
# Smoke test of the built desktop app against a real server: start the app,
# let its `KEYFOLD_AUTO*` hooks drive it, and check the backend log for
# what should have happened. Two runs:
#
#   profile  store a connection, connect, scan, disconnect, list, delete
#   switch   connect by URL, scan, switch to another database, scan
#
# The app's data, cache and config go to a temporary directory, so stored
# connections and indexes of the user are never touched. The stored
# connection has no password, so the OS keychain is not involved.
#
# Needs Docker and the app built with the frontend embedded:
#   npm run build && cargo build -p keyfold-app --features custom-protocol
#
# On a desktop the app's window shows for a few seconds per run. Without a
# display, in CI, or with KEYFOLD_SMOKE_XVFB=1 the app runs under Xvfb.
#
# Usage: scripts/smoke.sh [image]        default valkey/valkey:9
#   KEYFOLD_SMOKE_BIN      the app binary (default target/debug/keyfold-app)
#   KEYFOLD_SMOKE_PORT     port of the test server (default 16380)
#   KEYFOLD_SMOKE_TIMEOUT  seconds to wait per run (default 90)
#   KEYFOLD_SMOKE_KEEP=1   keep the logs and print where they are
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE="${1:-valkey/valkey:9}"
BIN="${KEYFOLD_SMOKE_BIN:-target/debug/keyfold-app}"
PORT="${KEYFOLD_SMOKE_PORT:-16380}"
TIMEOUT="${KEYFOLD_SMOKE_TIMEOUT:-90}"
SERVER=keyfold-smoke
URL="redis://127.0.0.1:${PORT}/0"
KEYS_DB0=300
KEYS_DB1=7

WORK="$(mktemp -d)"
APP_PID=""
XVFB_PID=""
STATUS=0

cleanup() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
  docker rm -f "$SERVER" >/dev/null 2>&1 || true
  if [ "${KEYFOLD_SMOKE_KEEP:-}" = 1 ]; then
    echo "logs kept in $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

die() {
  echo "smoke: $*" >&2
  exit 1
}

[ -x "$BIN" ] || die "$BIN is missing; build it with: npm run build && cargo build -p keyfold-app --features custom-protocol"

# --- the server, with a few keys in two databases -------------------------

cli() {
  docker exec -i "$SERVER" sh -c 'if command -v valkey-cli >/dev/null; then exec valkey-cli "$@"; else exec redis-cli "$@"; fi' cli -p "$PORT" "$@"
}

docker rm -f "$SERVER" >/dev/null 2>&1 || true
docker run -d --name "$SERVER" --network host "$IMAGE" \
  sh -c "if command -v valkey-server >/dev/null; then s=valkey-server; else s=redis-server; fi; exec \$s --port $PORT --bind 127.0.0.1 --save '' --appendonly no" >/dev/null
for _ in $(seq 1 100); do
  cli ping 2>/dev/null | grep -q PONG && break
  sleep 0.2
done
cli ping | grep -q PONG || die "the server from $IMAGE did not answer on port $PORT"

for i in $(seq 1 "$KEYS_DB0"); do echo "SET user:$i:profile x"; done | cli -n 0 >/dev/null
for i in $(seq 1 "$KEYS_DB1"); do echo "SET order:$i x"; done | cli -n 1 >/dev/null

# --- a display -------------------------------------------------------------

if [ -n "${CI:-}" ] || [ "${KEYFOLD_SMOKE_XVFB:-}" = 1 ] || { [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; }; then
  command -v Xvfb >/dev/null || die "no display and no Xvfb; install xvfb"
  display=":${KEYFOLD_SMOKE_DISPLAY:-99}"
  Xvfb "$display" -screen 0 1280x800x24 -nolisten tcp >"$WORK/xvfb.log" 2>&1 &
  XVFB_PID=$!
  for _ in $(seq 1 50); do
    [ -e "/tmp/.X11-unix/X${display#:}" ] && break
    sleep 0.1
  done
  kill -0 "$XVFB_PID" 2>/dev/null || die "Xvfb did not start: $(cat "$WORK/xvfb.log")"
  export DISPLAY="$display" GDK_BACKEND=x11
  unset WAYLAND_DISPLAY
  # no GPU under Xvfb
  export WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1
  echo "smoke: running under Xvfb on $display"
fi

# --- one run of the app ----------------------------------------------------

# run <name> <last line to wait for> <VAR=value>...
# Starts the app with the given hooks, waits for the line (or for the app to
# die, or the timeout), stops the app and leaves the log in $WORK/<name>.log
# without colour codes.
run() {
  local name=$1 until=$2
  shift 2
  local raw="$WORK/$name.raw" log="$WORK/$name.log" home="$WORK/$name"
  mkdir -p "$home/data" "$home/cache" "$home/config"
  env XDG_DATA_HOME="$home/data" XDG_CACHE_HOME="$home/cache" XDG_CONFIG_HOME="$home/config" \
    NO_COLOR=1 RUST_LOG=keyfold_app_lib=debug,webview=debug "$@" "$BIN" >"$WORK/$name.out" 2>"$raw" &
  APP_PID=$!
  local waited=0
  while [ "$waited" -lt $((TIMEOUT * 5)) ]; do
    grep -qF -- "$until" "$raw" 2>/dev/null && break
    kill -0 "$APP_PID" 2>/dev/null || break
    sleep 0.2
    waited=$((waited + 1))
  done
  # a moment for what the hook does after its last line
  sleep 1
  kill "$APP_PID" 2>/dev/null || true
  wait "$APP_PID" 2>/dev/null || true
  APP_PID=""
  sed 's/\x1b\[[0-9;]*m//g' "$raw" >"$log"
  echo "=== $name: $(grep -c '' "$log") log lines"
}

# expect <name> <text>: the run's log has a line with this text.
expect() {
  if grep -qF -- "$2" "$WORK/$1.log"; then
    echo "  ok       $2"
  else
    echo "  MISSING  $2"
    STATUS=1
  fi
}

# clean <name>: the run logged no error and did not panic.
clean() {
  local bad
  bad="$(grep -E ' ERROR |panicked at' "$WORK/$1.log" || true)"
  if [ -z "$bad" ]; then
    echo "  ok       no error lines"
  else
    echo "  ERRORS"
    echo "$bad" | sed 's/^/    /'
    STATUS=1
  fi
}

# The data directory the app reported must be the temporary one.
isolated() {
  if grep -F "profile store" "$WORK/$1.log" | grep -qF "$WORK/$1/data"; then
    echo "  ok       profile store under the temporary directory"
  else
    echo "  NOT ISOLATED: $(grep -F "profile store" "$WORK/$1.log" || echo "no profile store line")"
    STATUS=1
  fi
}

run profile "autoprofile deleted, 0 left" KEYFOLD_AUTOPROFILE="$URL"
isolated profile
expect profile "frontend loaded"
expect profile "stored connection saved"
expect profile "autoprofile connected smoke test db 0 $KEYS_DB0 keys"
expect profile "stored connection deleted"
expect profile "autoprofile deleted, 0 left"
clean profile

run switch "autoswitchdb scan complete" KEYFOLD_AUTOCONNECT="$URL" KEYFOLD_AUTOSWITCHDB=1
isolated switch
expect switch "frontend loaded"
expect switch "autoconnect layout"
expect switch "switched database"
expect switch "db 1, $KEYS_DB1 keys"
expect switch "autoswitchdb scan complete, $KEYS_DB1 keys in index"
clean switch

if [ "$STATUS" -ne 0 ]; then
  for name in profile switch; do
    echo "--- last lines of $name.log"
    tail -n 40 "$WORK/$name.log" || true
  done
  echo "smoke: FAILED"
else
  echo "smoke: ok ($IMAGE)"
fi
exit "$STATUS"
