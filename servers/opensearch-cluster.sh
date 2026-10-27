#!/usr/bin/env bash
# Local multi-node OpenSearch topology for the cluster / cross-DC tests.
#
#   main cluster   (ss-main):   a1, a2 in zone dc1; a3, a4 in zone dc2
#                               a1..a3 cluster_manager-eligible, a4 data-only
#   remote cluster (ss-remote): b1 in zone dc3 (separate cluster, for CCS)
#
# usage: opensearch-cluster.sh <up|down|status|env|stop-node N|kill-node N|start-node N>
#   N is a node name (a1..a4, b1).
# env: OS_HOME (default /tmp/os-3.9), SS_CLUSTER_ID (0..3, shifts all ports by
#      100*ID so several topologies can coexist), SS_CLUSTER_DIR, SS_HEAP.
set -euo pipefail

OS_HOME="${OS_HOME:-/tmp/os-3.9}"
ID="${SS_CLUSTER_ID:-0}"
DIR="${SS_CLUSTER_DIR:-/tmp/slopensearch-cluster-$ID}"
HEAP="${SS_HEAP:-512m}"
(( ID >= 0 && ID <= 3 )) || { echo "SS_CLUSTER_ID must be 0..3" >&2; exit 1; }
OFF=$((ID * 100))

NODES=(a1 a2 a3 a4 b1)
declare -A CLUSTER=([a1]=ss-main [a2]=ss-main [a3]=ss-main [a4]=ss-main [b1]=ss-remote)
declare -A ZONE=([a1]=dc1 [a2]=dc1 [a3]=dc2 [a4]=dc2 [b1]=dc3)
declare -A ROLES=([a1]=cluster_manager,data,ingest,remote_cluster_client [a2]=cluster_manager,data,ingest,remote_cluster_client
                  [a3]=cluster_manager,data,ingest,remote_cluster_client [a4]=data,ingest,remote_cluster_client
                  [b1]=cluster_manager,data,ingest,remote_cluster_client)
declare -A HTTP=([a1]=$((9411 + OFF)) [a2]=$((9412 + OFF)) [a3]=$((9413 + OFF))
                 [a4]=$((9414 + OFF)) [b1]=$((9421 + OFF)))
declare -A TRANSPORT=([a1]=$((9811 + OFF)) [a2]=$((9812 + OFF)) [a3]=$((9813 + OFF))
                      [a4]=$((9814 + OFF)) [b1]=$((9821 + OFF)))

MAIN_SEEDS="127.0.0.1:${TRANSPORT[a1]},127.0.0.1:${TRANSPORT[a2]},127.0.0.1:${TRANSPORT[a3]},127.0.0.1:${TRANSPORT[a4]}"

pidfile() { echo "$DIR/$1/pid"; }

# The JVM's command line carries the node's data path, so the process itself is the
# source of truth: a pidfile can be removed by a duplicate start failing on the data lock.
node_pid() { pgrep -f -- "-Epath.data=$DIR/$1/data( |$)" | head -1; }

running() { [[ -n "$(node_pid "$1")" ]]; }

start_node() {
  local n="$1"
  [[ -n "${CLUSTER[$n]:-}" ]] || { echo "unknown node $n" >&2; exit 1; }
  running "$n" && return 0
  mkdir -p "$DIR/$n/data" "$DIR/$n/logs"
  local args=(
    -Ecluster.name="${CLUSTER[$n]}-$ID"
    -Enode.name="$n"
    -Enode.roles="${ROLES[$n]}"
    -Enode.attr.zone="${ZONE[$n]}"
    -Ehttp.port="${HTTP[$n]}"
    -Etransport.port="${TRANSPORT[$n]}"
    -Enetwork.host=127.0.0.1
    -Epath.data="$DIR/$n/data"
    -Epath.logs="$DIR/$n/logs"
    -Ecluster.routing.allocation.disk.threshold_enabled=false
  )
  # the full distribution ships the security plugin; the suite talks plain HTTP
  if [[ -d "$OS_HOME/plugins/opensearch-security" ]]; then
    args+=(-Eplugins.security.disabled=true)
  fi
  if [[ "${CLUSTER[$n]}" == ss-main ]]; then
    args+=(-Ediscovery.seed_hosts="$MAIN_SEEDS" -Ecluster.initial_cluster_manager_nodes=a1,a2,a3)
  else
    args+=(-Ediscovery.type=single-node)
  fi
  OPENSEARCH_JAVA_OPTS="-Xms$HEAP -Xmx$HEAP" \
    "$OS_HOME/bin/opensearch" -d -p "$(pidfile "$n")" "${args[@]}" >"$DIR/$n/stdout.log" 2>&1
}

stop_node() {
  local n="$1" sig="$2" pid
  pid="$(node_pid "$n")"
  [[ -n "$pid" ]] || return 0
  kill "-$sig" "$pid"
  for _ in $(seq 120); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.5
  done
  if kill -0 "$pid" 2>/dev/null; then
    echo "node $n (pid $pid) did not stop" >&2
    exit 1
  fi
  rm -f "$(pidfile "$n")"
}

wait_health() {
  local port="$1" nodes="$2"
  for _ in $(seq 180); do
    if curl -sf "localhost:$port/_cluster/health?wait_for_nodes=$nodes&wait_for_status=green&timeout=1s" >/dev/null; then
      return 0
    fi
    sleep 1
  done
  echo "cluster on :$port did not reach $nodes nodes / green" >&2
  exit 1
}

case "${1:-}" in
  up)
    for n in "${NODES[@]}"; do start_node "$n"; done
    wait_health "${HTTP[a1]}" 4
    wait_health "${HTTP[b1]}" 1
    ;;
  down)
    for n in "${NODES[@]}"; do stop_node "$n" TERM; done
    ;;
  status)
    for n in "${NODES[@]}"; do
      if running "$n"; then s=up; else s=down; fi
      echo "$n ${CLUSTER[$n]}-$ID zone=${ZONE[$n]} http=${HTTP[$n]} transport=${TRANSPORT[$n]} $s"
    done
    ;;
  env)
    echo "export OS_CLUSTER_URLS=http://127.0.0.1:${HTTP[a1]},http://127.0.0.1:${HTTP[a2]},http://127.0.0.1:${HTTP[a3]},http://127.0.0.1:${HTTP[a4]}"
    echo "export OS_REMOTE_URL=http://127.0.0.1:${HTTP[b1]}"
    echo "export OS_CLUSTER_CTL='SS_CLUSTER_ID=$ID $(realpath "$0")'"
    ;;
  start-node) start_node "${2:?node name}" ;;
  stop-node) stop_node "${2:?node name}" TERM ;;
  kill-node) stop_node "${2:?node name}" KILL ;;
  *)
    echo "usage: $0 <up|down|status|env|stop-node N|kill-node N|start-node N>" >&2
    exit 1
    ;;
esac
