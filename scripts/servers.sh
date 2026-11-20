#!/usr/bin/env bash
# Start (or stop) the servers the live tests need for one image: a
# standalone primary, a replica of it, and a six-node cluster (three
# primaries, three replicas). Everything runs on the host network so the
# addresses the servers announce are reachable from the test process.
#
# For the SSH tunnel tests there are two more containers: a server on
# Docker's bridge network with no published port, and an sshd bastion
# (scripts/sshd) that knows it by the name `inner`. The test process cannot
# resolve that name, so reaching it proves the tunnel and the name
# resolution on the bastion. The bastion is on the host network, so it also
# reaches the cluster's announced addresses.
#
# Usage: scripts/servers.sh start <image>   prints the env the tests read
#        scripts/servers.sh stop
#        scripts/servers.sh env              prints the env again
set -euo pipefail

PORT="${KEYFOLD_MATRIX_PORT:-16390}"
REPLICA_PORT=$((PORT + 1))
CLUSTER_BASE=$((PORT + 10))
CLUSTER_NODES=6
SSH_PORT=$((PORT + 32))
SSH_DIR="$(cd "$(dirname "$0")/.." && pwd)/target/test-ssh"

env_lines() {
  echo "KEYFOLD_TEST_URL=redis://127.0.0.1:${PORT}/15"
  echo "KEYFOLD_TEST_REPLICA=1"
  echo "KEYFOLD_TEST_CLUSTER_URL=redis://127.0.0.1:${CLUSTER_BASE}/0"
  if [ -f "$SSH_DIR/id_ed25519" ]; then
    echo "KEYFOLD_TEST_SSH=tunnel@127.0.0.1:${SSH_PORT}"
    echo "KEYFOLD_TEST_SSH_KEY=${SSH_DIR}/id_ed25519"
    echo "KEYFOLD_TEST_SSH_PASSWORD=keyfold-test"
    echo "KEYFOLD_TEST_SSH_URL=redis://inner:6379/0"
  fi
}

stop() {
  rm -rf "$SSH_DIR"
  for n in keyfold-srv-primary keyfold-srv-replica keyfold-srv-cluster keyfold-srv-inner keyfold-srv-ssh; do
    docker rm -f "$n" >/dev/null 2>&1 || true
  done
}

wait_pong() {
  local name=$1 port=$2
  for _ in $(seq 1 100); do
    if docker exec "$name" sh -c "command -v valkey-cli >/dev/null && valkey-cli -p $port ping || redis-cli -p $port ping" 2>/dev/null | grep -q PONG; then
      return 0
    fi
    sleep 0.2
  done
  echo "server $name:$port did not answer" >&2
  return 1
}

# The server behind the bastion and the bastion itself. Without them the
# tunnel tests skip (no KEYFOLD_TEST_SSH in the env), so a failure here
# only warns.
start_bastion() {
  local image=$1 ip
  if ! docker build -q -t keyfold-test-sshd "$(dirname "$0")/sshd" >/dev/null; then
    echo "the sshd image did not build; the tunnel tests will be skipped" >&2
    return 0
  fi
  docker run -d --rm --name keyfold-srv-inner "$image" \
    sh -c 'exec $(command -v valkey-server || command -v redis-server) --port 6379 --bind 0.0.0.0 --protected-mode no --save "" --appendonly no' >/dev/null
  ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' keyfold-srv-inner)
  docker run -d --rm --name keyfold-srv-ssh --network host --add-host "inner:${ip}" keyfold-test-sshd "$SSH_PORT" >/dev/null
  mkdir -p "$SSH_DIR"
  for _ in $(seq 1 50); do
    if docker cp keyfold-srv-ssh:/keys/id_ed25519 "$SSH_DIR/id_ed25519" >/dev/null 2>&1 \
      && docker cp keyfold-srv-ssh:/keys/id_ed25519_pass "$SSH_DIR/id_ed25519_pass" >/dev/null 2>&1; then
      chmod 600 "$SSH_DIR"/id_ed25519*
      wait_pong keyfold-srv-inner 6379
      return 0
    fi
    sleep 0.2
  done
  echo "the bastion did not start; the tunnel tests will be skipped" >&2
  rm -rf "$SSH_DIR"
}

start() {
  local image=$1
  stop
  docker run -d --rm --name keyfold-srv-primary --network host "$image" \
    sh -c 'exec $(command -v valkey-server || command -v redis-server) --port '"$PORT"' --bind 127.0.0.1 --save "" --appendonly no' >/dev/null
  docker run -d --rm --name keyfold-srv-replica --network host "$image" \
    sh -c 'exec $(command -v valkey-server || command -v redis-server) --port '"$REPLICA_PORT"' --bind 127.0.0.1 --save "" --appendonly no \
      --replicaof 127.0.0.1 '"$PORT"' --replica-announce-ip 127.0.0.1 --replica-announce-port '"$REPLICA_PORT" >/dev/null
  # one container, six processes, then the cluster is formed with the cli
  docker run -d --rm --name keyfold-srv-cluster --network host "$image" sh -c '
    srv=$(command -v valkey-server || command -v redis-server)
    cli=$(command -v valkey-cli || command -v redis-cli)
    nodes=""
    for i in $(seq 0 '"$((CLUSTER_NODES - 1))"'); do
      p=$(('"$CLUSTER_BASE"' + i))
      $srv --port $p --bind 127.0.0.1 --cluster-enabled yes --cluster-config-file nodes-$p.conf \
        --cluster-announce-ip 127.0.0.1 --cluster-node-timeout 2000 --save "" --appendonly no --daemonize yes --logfile node-$p.log
      nodes="$nodes 127.0.0.1:$p"
    done
    sleep 1
    $cli --cluster create $nodes --cluster-replicas 1 --cluster-yes
    sleep infinity' >/dev/null
  start_bastion "$image"
  wait_pong keyfold-srv-primary "$PORT"
  wait_pong keyfold-srv-replica "$REPLICA_PORT"
  wait_pong keyfold-srv-cluster "$CLUSTER_BASE"
  # the replica must have synced, and the cluster must have converged
  for _ in $(seq 1 100); do
    if docker exec keyfold-srv-primary sh -c "command -v valkey-cli >/dev/null && valkey-cli -p $PORT info replication || redis-cli -p $PORT info replication" 2>/dev/null | grep -q "state=online"; then
      break
    fi
    sleep 0.2
  done
  for _ in $(seq 1 150); do
    if docker exec keyfold-srv-cluster sh -c "command -v valkey-cli >/dev/null && valkey-cli -p $CLUSTER_BASE cluster info || redis-cli -p $CLUSTER_BASE cluster info" 2>/dev/null | grep -q "cluster_state:ok"; then
      env_lines
      return 0
    fi
    sleep 0.2
  done
  echo "cluster did not converge" >&2
  docker logs keyfold-srv-cluster >&2 || true
  return 1
}

case "${1:-}" in
  start) start "${2:?image}" ;;
  stop) stop ;;
  env) env_lines ;;
  *) echo "usage: $0 start <image> | stop | env" >&2; exit 2 ;;
esac
