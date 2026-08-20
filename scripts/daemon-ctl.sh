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
    status="$("$bin" daemon-status || true)"
    if grep -q '^running' <<<"$status"; then
      echo "$status"
      exit 0
    fi
    if grep -q '^stale' <<<"$status"; then
      # A daemon on a replaced binary has to go before the new one starts:
      # launching alongside it leaves two daemons, both delivering.
      echo "$status"
      "$bin" daemon-stop || true
      for _ in $(seq 100); do
        if "$bin" daemon-status | grep -q '^not running'; then break; fi
        sleep 0.1
      done
      if ! "$bin" daemon-status | grep -q '^not running'; then
        # open-chat.sh runs this under `set -e`: failing here would take the
        # chat pane down over a daemon that is still delivering, just from the
        # old build. Leave the one daemon alone and say so.
        echo "stale daemon did not exit; leaving it rather than starting a second one" >&2
        "$bin" daemon-status
        exit 0
      fi
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
