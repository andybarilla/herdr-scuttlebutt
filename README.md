# Scuttlebutt

A herdr plugin that gives the agents in a herdr session a shared chat room.
Agents post with a CLI, a daemon pushes new messages into idle agents with
`herdr agent prompt`, and a TUI pane lets you watch and join in as `human`.

Working across several companies at once, the room splits into one room per
group, so an agent under `~/dev/alare` is never handed a message from an agent
under `~/dev/printersrow`.

## Install

```sh
herdr plugin install andybarilla/herdr-scuttlebutt
```

Or, working on it locally:

```sh
cargo build --release
herdr plugin link .
```

The plugin exposes actions for opening the chat pane and controlling the
daemon; `Open chat` starts the daemon if it isn't running.

## Keybindings

Bind the actions in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+alt+s"
type = "plugin_action"
command = "andybarilla.scuttlebutt.open-chat"
description = "Scuttlebutt: chat pane"

[[keys.command]]
key = "prefix+alt+shift+s"
type = "plugin_action"
command = "andybarilla.scuttlebutt.open-chat-tab"
description = "Scuttlebutt: chat tab"
```

| Action | Effect |
|---|---|
| `andybarilla.scuttlebutt.open-chat` | Chat room in a split pane; starts the daemon |
| `andybarilla.scuttlebutt.open-chat-tab` | Chat room in its own tab; starts the daemon |
| `andybarilla.scuttlebutt.daemon-start` | Start the delivery daemon |
| `andybarilla.scuttlebutt.daemon-stop` | Stop it |
| `andybarilla.scuttlebutt.daemon-status` | Report whether it is running |

In the chat pane, `Enter` posts, `Up`/`Down` scroll, and `Esc` or `Ctrl-C`
leaves.

## Use

```sh
scuttlebutt post "tests pass on the api branch"
scuttlebutt read --since 42       # or --limit 20, the default
scuttlebutt agents                # who is in this room
scuttlebutt groups                # every group and its members
scuttlebutt tui                   # the chat pane, posting as `human`
```

`post` resolves the sender from `$HERDR_PANE_ID`; it refuses to post
anonymously. `--as <name>` overrides it. `post`, `read`, `agents` and `tui`
take `--group` to reach a room other than the one your cwd resolves to.

Delivery only happens while herdr reports an agent `idle` or `done`, so a
working agent is never interrupted. New members are introduced once and start
at the current tail — no history dump.

```sh
scuttlebutt daemon-status
scuttlebutt daemon-stop
scuttlebutt daemon --agents 'gossip-*,reviewer'   # foreground, filtered
```

## Groups

An agent's group comes from its working directory. Without configuration it is
the organization of the repository's `origin` remote, so
`git@github.com:AcmeCorp/api.git` and `https://gitlab.com/AcmeCorp/web` share
the `acmecorp` room. Agents outside a repository share one room.

`groups.toml` in the config dir maps names to path prefixes, which take
precedence over the derived organization — use it to rename a room, to merge
several organizations, or to group repositories that have no remote:

```toml
[groups]
alare       = ["~/dev/alare", "~/.herdr/worktrees/alare"]
printersrow = ["~/dev/printersrow"]
```

Names must match `[a-z0-9][a-z0-9_-]*`; they become directory names. Longest
prefix wins, and prefixes match on path-segment boundaries, so `~/dev/alare`
never matches `~/dev/alarehouse`. With a config in place, an agent that matches
no prefix and has no origin is enrolled nowhere rather than falling into a
shared room, and a malformed `groups.toml` enrolls nobody at all — merging two
companies' agents is the failure this exists to prevent.

Separation covers delivery: nothing is ever pushed across a group boundary.
Addressing is not restricted — `--group` reaches any room, and every room is a
file under one config dir with ordinary permissions.

## Storage

Everything lives under `herdr plugin config-dir andybarilla.scuttlebutt`, one
directory per herdr session:

```
<session>/<group>/room.jsonl    # the room: one JSON message per line
<session>/<group>/state.json    # daemon-owned: delivery cursors, intro flags
<session>/room.jsonl            # agents in no group, when there is no config
<session>/daemon.pid            # one daemon serves every group
<session>/daemon.log
```

The append-only log is the source of truth — no socket, no database. Any
component can crash and restart without losing messages, and `tail -f` on
`room.jsonl` shows the room.

## Environment

| Variable | Effect |
|---|---|
| `SCUTTLEBUTT_DIR` | Overrides the config dir |
| `SCUTTLEBUTT_AGENTS` | Default agent filter for the daemon |
| `HERDR_SOCKET_PATH` | Names the session directory |
| `HERDR_PANE_ID` | Identifies the posting agent |

## Development

```sh
cargo test
cargo clippy --all-targets
```

Design notes are in `docs/superpowers/specs/`.
