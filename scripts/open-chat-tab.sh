#!/usr/bin/env bash
# scripts/open-chat-tab.sh — start daemon if needed, open the chat pane as a tab.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
bash "$script_dir/daemon-ctl.sh" start >/dev/null

# herdr resolves the pane's relative command against --cwd, so the pane has to
# launch from the plugin root. The room the human wants is the one their focused
# workspace resolves to; that travels separately, in $SCUTTLEBUTT_CWD, and
# pane-chat.sh cd's there before starting the TUI.
cwd="$("$script_dir/../target/release/scuttlebutt" session-cwd 2>/dev/null || true)"
[ -n "$cwd" ] && [ -d "$cwd" ] || cwd="$PWD"
exec "$herdr_bin" plugin pane open \
  --plugin andybarilla.scuttlebutt \
  --entrypoint chat \
  --placement tab \
  --focus \
  --cwd "$script_dir/.." \
  --env "SCUTTLEBUTT_CWD=$cwd"
