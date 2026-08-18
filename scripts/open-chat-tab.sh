#!/usr/bin/env bash
# scripts/open-chat-tab.sh — start daemon if needed, open the chat pane as a tab.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
bash "$script_dir/daemon-ctl.sh" start >/dev/null
exec "$herdr_bin" plugin pane open \
  --plugin andybarilla.scuttlebutt \
  --entrypoint chat \
  --placement tab \
  --focus \
  --cwd "$PWD"
