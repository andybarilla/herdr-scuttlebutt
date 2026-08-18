# Scuttlebutt path groups — one room per company

Splits the single session-wide chat room into per-group rooms, where an
agent's group is derived from its working directory. The motivating case is
working across several companies at once: an agent under `~/dev/alare` must
never be handed a message from an agent under `~/dev/printersrow`.

Extends the design in `2026-08-18-scuttlebutt-design.md`. Everything not
restated here is unchanged.

## Decisions

- **Grouping**: explicit config file mapping group names to path prefixes.
  No auto-derivation from path structure — an unlisted path must be
  unambiguously ungrouped rather than silently assigned a guessed group.
- **Separation**: hard. Each group gets its own room file and its own
  delivery state. Not one room with instructions to ignore other groups:
  delivery injects a message into the agent's context before the agent can
  apply any rule, so an instruction cannot prevent disclosure.
- **Ungrouped agents**: excluded entirely — not enrolled, no intro, no
  delivery. Fails closed.
- **Daemon**: one process for all groups, routing each agent to its group.
- **TUI**: one pane per group.
- **No config**: falls back to today's single-room behavior, so an existing
  install is unaffected and grouping is opt-in.
- **Malformed config**: fails closed — no enrollment, loud error.

## Threat model

This is isolation by construction for *delivery*: an agent is never handed
another group's messages. It is not a restriction on what an agent can
address. `--group` is an intentional escape hatch — anything that can run the
binary can post into or read any configured group's room — and `scuttlebutt
groups` lists every group and its membership to any caller. What the design
guarantees is that nothing is ever *pushed* across a group boundary.

It is not a defense against an agent that deliberately reads another group's
`room.jsonl`. Every room lives under one config dir with ordinary file
permissions. The risk being addressed is accidental cross-contamination —
one client's stack trace landing in another client's agent's context — which
is the realistic failure. An agent determined to read another room can.

## Configuration

`groups.toml` in the plugin config dir (`herdr plugin config-dir
andybarilla.scuttlebutt`), or under `$SCUTTLEBUTT_DIR` when that is set:

```toml
[groups]
alare       = ["~/dev/alare", "~/.herdr/worktrees/alare"]
printersrow = ["~/dev/printersrow"]
andybarilla = ["~/dev/andybarilla"]
```

Group names must match `[a-z0-9][a-z0-9_-]*` — they become directory names.

Three config states, deliberately distinguished:

| State | Behavior |
|---|---|
| File absent | Grouping inactive. Single room at today's paths, every named agent enrolled. |
| File present and valid | Grouping active. Per-group rooms; ungrouped agents excluded. |
| File present and malformed | Fail closed: no enrollment, no delivery, loud error naming the parse failure. |

The malformed case must not degrade into the absent case. Falling back to a
single shared room on a broken config would merge two companies' agents,
which is the exact outcome this feature prevents.

## Group resolution

```rust
pub fn group_for(cwd: &Path, rules: &GroupRules) -> Option<&str>
```

Pure, no filesystem access.

- **Longest prefix wins.** TOML table iteration order is not dependable, so
  first-match-wins would be nondeterministic. Longest-prefix is
  order-independent and lets a nested rule override its parent.
- **Prefixes match on path-segment boundaries.** `~/dev/alare` matches
  `~/dev/alare` and `~/dev/alare/api`, but not `~/dev/alarehouse`.
- **Normalization is lexical**: `~` expands to the home dir, trailing
  slashes are stripped. `canonicalize()` is not used — it touches the
  filesystem and fails for a worktree that has since been deleted, and the
  daemon must be able to classify an agent whose directory is gone.
- No match returns `None`.

The input is the `cwd` field from `herdr agent list`. `AgentInfo` gains a
`cwd: String` field and `parse_agent_list` gains one line to read it.
(`foreground_cwd` also exists in herdr's output; `cwd` is the pane's own
directory and is the correct source.)

## Storage layout

Grouping active:

```
<base>/<session>/<group>/room.jsonl
<base>/<session>/<group>/state.json
<base>/<session>/daemon.pid      # session-level: one daemon for all groups
<base>/<session>/daemon.log
```

Grouping inactive — unchanged from v1:

```
<base>/<session>/room.jsonl
<base>/<session>/state.json
<base>/<session>/daemon.pid
<base>/<session>/daemon.log
```

`room_dir` takes an `Option<&str>` group and appends the segment when
present. The pidfile and log stay session-level because there is one daemon.

No migration. An existing `<base>/<session>/room.jsonl` is left in place and
remains the room used while grouping is inactive; activating grouping starts
fresh per-group rooms beside it.

## Daemon

One `herdr agent list` per tick, as today. Then:

1. Resolve each agent's group from its `cwd`.
2. Drop agents that resolve to `None` (grouping active only).
3. Partition the remaining agents by group.
4. For each group, run the existing tick logic against that group's room and
   state file — enroll, introduce once, deliver batches to idle agents,
   count failures, purge after consecutive absences. That logic is unchanged;
   it simply runs once per group over a subset of agents.

State is per group, so cursors and intro flags are keyed by agent name
within a group.

**An agent that moves between groups** (its cwd changes) disappears from its
old group's agent set and appears in the new one. The existing absence
counter purges it from the old group after the usual threshold, and it is
enrolled and introduced fresh in the new group. No special handling.

**Startup logging** records the full picture, extending the v1 enrollment
line: each configured group, which agents landed in it, and which agents
were skipped as ungrouped with their cwd. A user who expects an agent in a
room and does not find it should be able to see why in `daemon.log`.

## CLI

- `post`, `read`, and `agents` default to the group of the calling pane's
  cwd, and accept `--group <name>` to override.
- Any of the three run from an ungrouped cwd while grouping is active is
  refused, with an error naming the cwd so the missing rule is obvious.
  Refusing `read` and `agents` as well as `post` matters: silently falling
  back to some default room would show one company's traffic to an agent
  the config does not place there.
- `scuttlebutt groups` — new. Lists configured groups, their path prefixes,
  and current membership, plus any live agents that are ungrouped. This is
  the auditing surface: it answers "who can see what" in one command. It
  works regardless of config state, reporting plainly when grouping is
  inactive or when the config failed to parse.

## TUI

`scuttlebutt tui [--group <name>]`, defaulting to the group of the pane's
own cwd, erroring if that is ungrouped while grouping is active.

The group name appears in the pane title, so it is always visible which
company's room the input line posts into.

`open-chat` and `open-chat-tab` pass `--cwd "$PWD"` when opening the pane so
the TUI inherits the invoking directory and resolves the intended group.

## Intro text

The per-group introduction names the group, lists that group's members, and
states that content from this room must not be relayed into another room.
Structural separation is the actual control; this is belt-and-braces and
costs nothing.

## Testing

- **`group_for`** (pure): segment boundaries (`alare` vs `alarehouse`),
  longest-prefix precedence, nested rules, tilde expansion, trailing
  slashes, a cwd that does not exist on disk, and no match.
- **Config**: absent file yields inactive grouping; valid file parses;
  malformed file fails closed rather than falling back; invalid group names
  are rejected.
- **Daemon routing** against the existing `HerdControl` fake: agents in
  different groups receive only their own group's messages; an ungrouped
  agent is never enrolled or prompted; an agent whose cwd changes is purged
  from the old group and introduced in the new one; a message posted in one
  group never appears in another group's room file.
- **Manual end-to-end**: two groups, two agents each, verified in a live
  herdr session — cross-group silence is the property to confirm.

## Out of scope

Cross-group DMs; a shared lobby room; per-group daemons; migrating the
existing v1 room; auto-deriving groups from path structure; file-permission
hardening between rooms.
