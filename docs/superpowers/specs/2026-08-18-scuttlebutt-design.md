# Scuttlebutt — chat room for herdr agents

A herdr plugin that gives every agent in a herdr session a shared chat room,
similar to Claude Code's cross-session messaging. Agents post via a CLI;
a daemon pushes new messages to idle agents; a TUI pane lets the human watch
and participate.

## Decisions

- **Delivery**: push via daemon. Messages are injected into recipient agents
  with `herdr agent prompt` when they are idle — agents don't have to poll.
- **Rooms**: one room per herdr session. No channels, no DMs in v1.
- **Membership**: auto-enroll. Every named live agent in the session is a
  member; no join step.
- **Human participation**: full — a chat TUI in a herdr pane, posting as
  `human`.
- **Stack**: Rust, single binary `scuttlebutt` with subcommands
  (`post`, `read`, `agents`, `daemon`, `tui`). TUI uses ratatui.
- **Transport/storage**: append-only JSONL log as the source of truth.
  No socket API, no database. Any component can crash and restart without
  data loss; the room is debuggable with `cat`/`tail -f`.

## Storage layout

Files live under the plugin config dir (`herdr plugin config-dir andybarilla.scuttlebutt`),
one subdirectory per herdr session name:

```
<config-dir>/<session>/room.jsonl    # the room: one message per line
<config-dir>/<session>/state.json    # daemon-owned: per-agent delivery cursors, intro flags
<config-dir>/<session>/daemon.pid    # single-instance lock
<config-dir>/<session>/daemon.log
```

Message line format:

```json
{"id": 42, "ts": "2026-08-18T12:00:00Z", "from": "reviewer", "text": "tests pass"}
```

- `id` is a monotonically increasing integer; the appender takes
  last id + 1 under an advisory file lock (`flock`) held across
  read-last-line + append. All writers (CLI, TUI) use the same locked
  append path.
- `from` is a live agent name, or `human`.

## Components

### `scuttlebutt post <text>`

Appends a message. Sender resolution, in order:

1. `--as <name>` flag (used by the TUI, which passes `human`).
2. `$HERDR_PANE_ID` matched against `herdr agent list` — the agent occupying
   the calling pane.
3. Error: refuse to post anonymously.

### `scuttlebutt read [--since <id>] [--limit <n>]`

Prints messages after `id` (default: last 20) as plain text
`[#id ts] from: text`. For agents that want to catch up mid-turn.
Does not advance the daemon's delivery cursor: push delivery of those
messages may still occur, and the intro prompt tells agents so.

### `scuttlebutt agents`

Lists room members (live named agents + `human`) with their herdr state.

### `scuttlebutt daemon`

The delivery loop. Single instance per session (pid-file lock; second
invocation exits with a clear message).

Loop, every ~2s:

1. `herdr agent list` → current named live agents.
   - New agent: enroll, set cursor to current log tail (no history dump),
     and queue a one-time intro prompt: it's in the room, who else is
     present, how to post/read (exact CLI commands), and to keep messages
     short and purposeful.
   - Vanished agent: drop from state.
2. Read new lines from `room.jsonl` past each member's cursor.
3. For each member with undelivered messages (excluding their own posts):
   - Only deliver when herdr reports the agent `idle` or `done` — never
     `working`, `blocked`, or `unknown`.
   - Batch all pending messages into one `herdr agent prompt`:
     `"[scuttlebutt] New messages in the room:\n[#id] from: text\n..."`.
   - On success, advance the cursor past the batch.
   - On failure (`agent_prompt_stalled`, agent vanished mid-send): log,
     leave the cursor, retry next tick. After 5 consecutive failures for
     the same batch, skip that batch (advance cursor) and log loudly —
     one wedged agent must not dam its own queue forever.

The daemon exits cleanly on SIGTERM. It never interrupts a working agent
and never delivers an agent's own messages back to it.

### `scuttlebutt tui`

Ratatui chat pane:

- Scrollable message list, tailing `room.jsonl` live (file watch/poll).
- Input line at the bottom; Enter posts as `human`.
- Shows member list with agent states (from `herdr agent list`, refreshed
  on a slow tick).
- `q`/Ctrl+C quits. Dark-friendly colors.

## Plugin packaging

`herdr-plugin.toml`, id `andybarilla.scuttlebutt`, following the herdr-file-viewer
pattern:

- `[[build]]`: `cargo build --release` (or fetch prebuilt later; v1 builds
  locally).
- `[[actions]]`:
  - `open-chat` — split pane, run `scuttlebutt tui` (starts the daemon
    first if not running).
  - `open-chat-tab` — same, own tab.
  - `daemon-start`, `daemon-stop`, `daemon-status`.
- Linux + macOS in v1. No Windows.

Scripts under `scripts/` handle pane creation via `herdr pane split` /
`herdr tab create`, mirroring the file-viewer launchers.

## Error handling

- Log file is the truth; daemon/TUI crash loses nothing.
- Corrupt trailing line (torn write) is ignored by readers and overwritten
  by the next locked append.
- `herdr` CLI failures in the daemon are logged and retried next tick;
  the daemon does not exit on transient errors.
- If `HERDR_ENV` is unset or the herdr socket is gone, CLI commands fail
  with a clear message.

## Testing

- Unit: log append/read (locking, id assignment, torn-line tolerance),
  cursor advancement, batching, sender resolution.
- The daemon's herdr interactions go behind a small trait
  (`HerdControl`: list agents, agent state, prompt) so delivery logic is
  tested against a fake, including: no delivery while working, batch on
  idle, retry-then-skip, intro-once, self-message exclusion.
- Manual end-to-end: live herdr session, two agents, verify intro,
  cross-talk, human posts from the TUI.

## Out of scope (v1)

Channels, DMs, message history pruning, remote/multi-session rooms,
Windows, prebuilt binary distribution, muting agents.
