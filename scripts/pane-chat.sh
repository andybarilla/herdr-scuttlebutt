#!/usr/bin/env bash
# scripts/pane-chat.sh — the chat pane's entrypoint.
# herdr resolves a pane's relative command against the pane cwd, so the pane
# must launch from the plugin root. The room the human wants is the one their
# focused workspace resolves to, which arrives as $SCUTTLEBUTT_CWD; the TUI
# resolves its group from its own cwd, so switch there before exec'ing.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
bin="$script_dir/../target/release/scuttlebutt"
cwd="${SCUTTLEBUTT_CWD:-}"
[ -n "$cwd" ] && [ -d "$cwd" ] && cd "$cwd"
exec "$bin" tui
