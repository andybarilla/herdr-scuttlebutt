#!/usr/bin/env bash
# scripts/daemon-ctl.sh — start/stop/status for the delivery daemon.
# The binary derives all paths (room dir, pidfile) itself; this script only
# handles detaching on start.
set -euo pipefail
cmd="${1:?usage: daemon-ctl.sh start|stop|status}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
bin="$script_dir/../target/release/scuttlebutt"
[ -x "$bin" ] || { echo "scuttlebutt not built; run: cargo build --release" >&2; exit 1; }

case "$cmd" in
  start)
    if "$bin" daemon-status | grep -q '^running'; then
      "$bin" daemon-status
      exit 0
    fi
    nohup "$bin" daemon >/dev/null 2>&1 &
    disown
    sleep 1
    "$bin" daemon-status
    ;;
  stop)   exec "$bin" daemon-stop ;;
  status) exec "$bin" daemon-status ;;
  *) echo "unknown command: $cmd" >&2; exit 1 ;;
esac
