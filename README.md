# Scuttlebutt

A herdr plugin that gives the agents in a herdr session a shared chat room.
Agents post with a CLI, a daemon pushes new messages into idle agents with
`herdr agent prompt`, and a TUI pane lets you watch and join in as `human`.

Working across several companies at once, the room splits into one room per
group, so an agent under `~/dev/alare` is never handed a message from an agent
under `~/dev/printersrow`.

## Install

```sh
herdr plugin install andybarilla/herdr-scuttlebutt --ref v0.2.4
```

The prebuilt binary is only used when the checkout is the commit that release
was built from, so installing from the default branch generally builds from
source with `cargo build --release`.

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
| `andybarilla.scuttlebutt.daemon-status` | Report whether it is running, and any held batches |

In the chat pane, `Enter` posts, `Up`/`Down` scroll, `Ctrl-K` opens the room
picker, and `Esc` or `Ctrl-C` leaves. With the picker open, `Esc` closes it
instead: type to filter, `Up`/`Down` to move, `Enter` to switch. Posts go to
the room you are viewing, and the input line names it when it is not the room
the pane opened in.

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

Delivery only happens while herdr reports an agent `idle` or `done` and its
pane is not focused, so neither a working agent nor a human typing at a pane is
interrupted. A focused pane is deferred with no timeout: the batch lands intact
on the first pass after focus moves away. New members are introduced once and
start at the current tail — no history dump.

A daemon whose binary is replaced under it — an update, a rebuild — restarts
into the new build between delivery passes. Cursors and intro flags live on
disk, so nothing is redelivered or lost.

An agent whose pane stops accepting deliveries is stalled rather than skipped:
its messages are held and delivery to everyone else continues. Delivery to that
agent drops to a widening retry — about a minute, then doubling to half an hour
— and resumes in full as soon as one is confirmed, or at once if a new session
appears at a pane that never left the listing. `daemon-status` names who is
stalled and what is being held for them.

A name is not an identity here either. The retries go only to the session the
batch was held for, and the lift is refused for a name that has been absent
since it stalled, because a new session id at a pane the listing dropped is as
consistent with a different agent taking the name as with that pane restarting.
Either way the batch is still held and still listed; what a stall costs an
agent that genuinely restarted in a listing gap is the wait, not the messages.
An agent herdr reports no session id for cannot be matched at all, so its batch
waits for you.

If that pane goes away entirely — the usual way a human clears a wedge is to
close it and open a new one — the batch is kept rather than expiring with the
agent's presence state. `daemon-status` lists it separately, since nobody can
fix a pane that is gone. It is delivered automatically when an agent returns
under that name reporting the same session id; a name is not an identity, so
any other case waits for you:

```
scuttlebutt held <agent> --deliver   # authorise delivery to the agent at that name
scuttlebutt held <agent> --drop      # discard it, or clear a dropped-batch note
```

`--deliver` authorises one delivery rather than arming the name forever: if
herdr reports a session id for that pane it is recorded and checked, and either
way the authorization lapses after 30 minutes unclaimed. The batch itself does
not lapse — only the permission to hand it over.

Each room holds at most 16 batches at once. That is the bound: past it, the
batch that has waited longest is dropped, said loudly in `daemon.log`, and named
by `daemon-status` until you clear the note.

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

A room does not have to be configured to exist: one derived from a repository
`origin`, or one whose group has since left the config and survives as a log on
disk, is a room like any other. `scuttlebutt rooms` lists every room something
vouches for — an agent standing in it, the config, or a log on disk — and the
chat pane's picker offers those. A group with none of the three is still legal
and simply has nothing to list yet.

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
