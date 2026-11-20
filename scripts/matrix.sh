#!/usr/bin/env bash
# Run the backend test suite against every server in the CI matrix, one
# image at a time: a standalone primary with a replica and a six-node
# cluster per image (scripts/servers.sh). Needs Docker.
# Usage: scripts/matrix.sh [cargo test args]
set -euo pipefail
cd "$(dirname "$0")/.."

MATRIX=(
  "redis:7.2|redis"
  "redis:8|redis"
  "valkey/valkey:8|valkey"
  "valkey/valkey:9|valkey"
)

status=0
for entry in "${MATRIX[@]}"; do
  image="${entry%%|*}"
  expect="${entry##*|}"
  echo "=== $image"
  if ! scripts/servers.sh start "$image" >/dev/null; then
    echo "=== $image: servers did not start"
    status=1
    continue
  fi
  # shellcheck disable=SC2046
  if env $(scripts/servers.sh env | xargs) KEYFOLD_EXPECT_SERVER="$expect" cargo test --workspace "$@"; then
    echo "=== $image: ok"
  else
    echo "=== $image: FAILED"
    status=1
  fi
  scripts/servers.sh stop
done
exit $status
