# Scuttlebutt path groups — one room per company

Splits the single session-wide chat room into per-group rooms, where an
agent's group is derived from its working directory. The motivating case is
working across several companies at once: an agent under `~/dev/alare` must
never be handed a message from an agent under `~/dev/printersrow`.

Extends the design in `2026-08-18-scuttlebutt-design.md`. Everything not
restated here is unchanged.

## Decisions

- **Grouping**: explicit config file mapping group names to path prefixes,
  falling back to the repository's `origin` organization for a cwd no prefix
  claims. Nothing is derived from path structure — a guessed group from
  directory names would be silent and wrong; an org comes from the repo
  itself.
- **Separation**: hard. Each group gets its own room file and its own
  delivery state. Not one room with instructions to ignore other groups:
  delivery injects a message into the agent's context before the agent can
  apply any rule, so an instruction cannot prevent disclosure.
- **Ungrouped agents** (no prefix, no repo origin): excluded entirely under
  an active config — not enrolled, no intro, no delivery. Fails closed. With
  no config they share the single v1 room instead, so a non-repo agent does
  not go dark just because someone else's repo created an org room.
- **Daemon**: one process for all groups, routing each agent to its group.
- **TUI**: one pane per group.
- **No config**: each agent's group is its repo's origin organization; agents
  outside a repo share today's single room.
- **Malformed config**: fails closed — no enrollment, loud error.

## Threat model

This is isolation by construction for *delivery*: an agent is never handed
another group's messages. Addressing is unrestricted: `--group` is an
intentional escape hatch, so anything that can run the binary can post into or
read any configured group's room, and `scuttlebutt groups` lists every group
and its membership to any caller. The guarantee is that nothing is ever
*pushed* across a group boundary.

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
| File absent | Groups derived from repo origins; agents outside a repo share the single room at today's paths. |
| File present and valid | Prefixes first, repo origin for anything they do not claim; agents with neither excluded. |
| File present and malformed | Fail closed: no enrollment, no delivery, loud error naming the parse failure. |

The malformed case must not degrade into the absent case. Falling back to a
single shared room on a broken config would merge two companies' agents,
which is the exact outcome this feature prevents.

## Group resolution

```rust
pub fn group_for(cwd: &Path, rules: &GroupRules) -> Option<&str>          // prefixes
pub fn resolve(cwd, &Grouping, &mut OrgCache) -> Option<String>           // prefixes, then origin
```

`group_for` is pure, no filesystem access.

- **Longest prefix wins.** TOML table iteration order is not dependable, so
  first-match-wins would be nondeterministic. Longest-prefix is
  order-independent and lets a nested rule override its parent.
- **Prefixes match on path-segment boundaries.** `~/dev/alare` matches
  `~/dev/alare` and `~/dev/alare/api`, but not `~/dev/alarehouse`.
- **Normalization is lexical**: `~` expands to the home dir, trailing
  slashes are stripped. `canonicalize()` is not used — it touches the
  filesystem and fails for a worktree that has since been deleted, and the
  daemon must be able to classify an agent whose directory is gone.
- No match falls through to the origin organization; if that yields nothing
  too, the result is `None`.

### Origin fallback

`git -C <cwd> config --get remote.origin.url`, then the first path segment
after the host: `git@github.com:AcmeCorp/api.git` and
`https://gitlab.com/AcmeCorp/web` both give `acmecorp`. The name is
lowercased, anything outside `[a-z0-9_-]` becomes `-`, and leading characters
are dropped until one that may start a group name. A local-path remote has no
owner and yields `None`, as does a directory in no repo, with no `origin`, or
gone from disk.

Two forges with the same owner name share a room. That is deliberate: the
same organization on GitHub and GitLab is the same company.

A derived name equal to a configured group name is that group's room. A
config entry is therefore also the way to rename an org's room or to merge
several orgs into one.

Results are cached per cwd for 5 minutes — the daemon resolves every agent on
every 2s tick, and without the cache each tick spawns one `git` per agent.
The TTL exists because worktrees are created and deleted under a
long-running daemon.

**`--group` under no config.** Org groups are not enumerable — a room exists
as soon as an agent with that origin starts — so an explicit `--group` is
checked for legality, not membership. A typo opens an empty room rather than
erroring; every TUI pane titles its room, which is where that shows up.

The input is the `cwd` field from `herdr agent list`. `AgentInfo` gains a
`cwd: String` field and `parse_agent_list` gains one line to read it.
(`foreground_cwd` also exists in herdr's output; `cwd` is the pane's own
directory and is the correct source.)

## Storage layout

Any group, prefix- or origin-derived:

```
<base>/<session>/<group>/room.jsonl
<base>/<session>/<group>/state.json
<base>/<session>/daemon.pid      # session-level: one daemon for all groups
<base>/<session>/daemon.log
```

Ungrouped agents — the v1 layout, used by whatever resolves to no group
while there is no config:

```
<base>/<session>/room.jsonl
<base>/<session>/state.json
<base>/<session>/daemon.pid
<base>/<session>/daemon.log
```

`room_dir` takes an `Option<&str>` group and appends the segment when
present. The pidfile and log stay session-level because there is one daemon.

No migration. An existing `<base>/<session>/room.jsonl` is left in place and
remains the room for agents in no group; org- and prefix-derived rooms start
fresh beside it.

## Daemon

One `herdr agent list` per tick, as today. Then:

1. Resolve each agent's group from its `cwd`: prefix, else repo origin.
2. Drop agents that resolve to `None` (grouping active only; with no config
   they form the shared room).
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
