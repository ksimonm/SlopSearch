#!/usr/bin/env bash
# Restart the server listening on <port>: stop it, then start <command...> detached.
#   restart.sh [--signal TERM|KILL] <port> <command...>
# --signal KILL simulates a crash (no graceful flush on shutdown).
set -euo pipefail

sig=TERM
if [[ "${1:-}" == "--signal" ]]; then
  sig="${2:?--signal needs a value}"
  shift 2
fi
port="${1:?usage: restart.sh [--signal TERM|KILL] <port> <command...>}"
shift
[[ $# -gt 0 ]] || { echo "usage: restart.sh [--signal TERM|KILL] <port> <command...>" >&2; exit 1; }

listening() { ss -ltnpH "sport = :$port" | grep -oP 'pid=\K[0-9]+' | sort -u; }

pids="$(listening || true)"
if [[ -n "$pids" ]]; then
  kill "-$sig" $pids
  for _ in $(seq 120); do
    [[ -z "$(listening || true)" ]] && break
    sleep 0.5
  done
  if [[ -n "$(listening || true)" ]]; then
    echo "port $port is still busy after SIG$sig" >&2
    exit 1
  fi
fi

setsid nohup "$@" >"${RESTART_LOG:-/dev/null}" 2>&1 < /dev/null &
