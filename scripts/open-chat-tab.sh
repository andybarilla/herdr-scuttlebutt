#!/usr/bin/env bash
# scripts/open-chat-tab.sh — start daemon if needed, open the chat pane as a tab.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
bash "$script_dir/daemon-ctl.sh" start >/dev/null

# herdr runs plugin actions from the plugin's own directory, so $PWD would pin
# the pane to this repo's group. The room the human wants is the one their
# focused workspace resolves to.
cwd="$("$script_dir/../target/release/scuttlebutt" session-cwd 2>/dev/null || true)"
[ -n "$cwd" ] && [ -d "$cwd" ] || cwd="$PWD"
exec "$herdr_bin" plugin pane open \
  --plugin andybarilla.scuttlebutt \
  --entrypoint chat \
  --placement tab \
  --focus \
  --cwd "$cwd"
