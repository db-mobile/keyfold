#!/usr/bin/env bash
# Benchmark a full scan of a generated database (CONCEPT section 9).
# Usage: scripts/bench.sh [keys=10000000] [image=redis:8] [extra scan args...]
set -euo pipefail
cd "$(dirname "$0")/.."
KEYS="${1:-10000000}"
IMAGE="${2:-redis:8}"
shift $(( $# > 2 ? 2 : $# ))
PORT="${KEYFOLD_BENCH_PORT:-16391}"
NAME="keyfold-bench"
INDEX=".local/bench/${IMAGE//[^a-zA-Z0-9]/_}_${KEYS}.sqlite"

cargo build --release --workspace
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --rm --name "$NAME" -p "127.0.0.1:${PORT}:6379" "$IMAGE" --save "" --appendonly no >/dev/null
sleep 2
URL="redis://127.0.0.1:${PORT}/0"
echo "=== generating $KEYS keys into $IMAGE"
./target/release/keyfold gen "$URL" --keys "$KEYS" --flush
echo "=== cold scan (fresh index)"
rm -f "${INDEX%.sqlite}".*sqlite* "$INDEX"
/usr/bin/time -f "wall %e s, max rss %M KB, fs writes %O blocks" ./target/release/keyfold scan "$URL" --index "$INDEX" --profile fast "$@"
echo "=== rescan (index already complete)"
/usr/bin/time -f "wall %e s, max rss %M KB" ./target/release/keyfold scan "$URL" --index "$INDEX" --profile fast "$@"
echo "=== tree windows"
./target/release/keyfold tree "$INDEX" "user:" --limit 200 --sort numeric | tail -1
./target/release/keyfold tree "$INDEX" "order:" --limit 200 --sort name | tail -1
docker rm -f "$NAME" >/dev/null 2>&1 || true
