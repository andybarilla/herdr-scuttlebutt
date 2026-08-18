# Scuttlebutt Path Groups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the single session-wide chat room into per-group rooms, where an agent's group is derived from its working directory via an explicit config file, so agents working for different companies never receive each other's messages.

**Architecture:** A new `groups` module owns config loading and a pure `group_for(cwd, rules)` resolver. `paths::room_dir` gains an optional group segment. The daemon resolves each agent's group once per tick, partitions agents by group, and runs the existing unchanged `tick` logic once per group against that group's own room and state file. CLI and TUI commands resolve the caller's group from its own cwd and accept a `--group` override.

**Tech Stack:** Rust 2021. Existing: anyhow, clap 4 (derive), serde/serde_json, fs2, chrono, ratatui 0.29, crossterm 0.28, signal-hook; tempfile in dev-dependencies. **New (user-approved):** `toml = "0.8"` for config parsing, `unicode-width = "0.2"` for the TUI wrapping fix.

**Spec:** `docs/superpowers/specs/2026-08-18-scuttlebutt-groups-design.md`

## Global Constraints

- Config file is `groups.toml` in the room base dir (`paths::base_dir()`), NOT inside the session dir — group rules are machine-wide, not per-session.
- Three config states are distinguished and must never collapse into each other: **absent** → grouping inactive, today's single room, every named agent enrolled; **valid** → grouping active, per-group rooms, ungrouped agents excluded entirely; **malformed** → fail closed, no enrollment, no delivery, loud error naming the parse failure.
- Group names must match `^[a-z0-9][a-z0-9_-]*$` — they become directory names.
- `group_for` is pure: no filesystem access, no `canonicalize()`. Lexical normalization only (tilde expansion, trailing-slash strip).
- Longest matching prefix wins. Prefix matches only on path-segment boundaries: `~/dev/alare` matches `~/dev/alare` and `~/dev/alare/api`, never `~/dev/alarehouse`.
- Ungrouped agents while grouping is active: not enrolled, no intro, no delivery. `post`, `read`, and `agents` from an ungrouped cwd are refused with an error naming the cwd.
- Storage when grouping is active: `<base>/<session>/<group>/room.jsonl` and `<base>/<session>/<group>/state.json`. `daemon.pid` and `daemon.log` stay at `<base>/<session>/` — one daemon serves all groups.
- No migration: an existing `<base>/<session>/room.jsonl` is left untouched and remains the inactive-grouping room.
- Delivery rules from v1 are unchanged and must stay passing: deliver only on `idle`/`done`; never deliver an agent's own messages back; one batched prompt; 5 consecutive failures for the same batch skips it; 3 consecutive absences purge; 2 consecutive deliverable sightings before the first prompt.
- Tests must never invoke a live herdr. Use the existing `HerdControl` fake and tempdirs.
- Run tests as `cargo test -- --test-threads=1`. `cargo build` and `cargo clippy --all-targets` must stay warning-free — they are clean today.
- The Rust toolchain is rustup at `~/.cargo/bin`, NOT on the default PATH. Every shell command starts with `export PATH="$HOME/.cargo/bin:$PATH"`.
- No README or other explanatory documentation.

## File Structure

- **Create `src/groups.rs`** — config struct, TOML loading with the three-state contract, and the pure `group_for` resolver. One responsibility: "which group does this path belong to".
- **Modify `src/paths.rs`** — `room_dir` takes `Option<&str>`.
- **Modify `src/herd.rs`** — `AgentInfo` gains `cwd`; `parse_agent_list` reads it.
- **Modify `src/daemon.rs`** — per-group partitioning in `run`; `tick` itself is untouched.
- **Modify `src/cli.rs`, `src/main.rs`** — `--group` flags, group resolution for callers, new `groups` subcommand.
- **Modify `src/tui.rs`** — `--group`, group in the title, and the unicode-width wrapping fix.
- **Modify `scripts/open-chat.sh`, `scripts/open-chat-tab.sh`** — pass `--cwd "$PWD"`.

---

### Task 1: Groups config and the pure resolver

**Files:**
- Create: `src/groups.rs`
- Modify: `Cargo.toml` (add `toml = "0.8"`), `src/main.rs` (add `mod groups;`)

**Interfaces:**
- Consumes: `paths::base_dir() -> Result<PathBuf>`
- Produces:
  - `pub enum Grouping { Inactive, Active(GroupRules), Broken(String) }`
  - `pub struct GroupRules { rules: Vec<(String, PathBuf)> }` with `pub fn names(&self) -> Vec<&str>` and `pub fn prefixes_for(&self, group: &str) -> Vec<&Path>`
  - `pub fn group_for<'a>(cwd: &Path, rules: &'a GroupRules) -> Option<&'a str>`
  - `pub fn load(base: &Path) -> Grouping`
  - `pub fn expand_tilde(p: &str) -> PathBuf`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` under `[dependencies]`, add:

```toml
toml = "0.8"
```

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo build`
Expected: builds, downloading `toml`.

- [ ] **Step 2: Write the failing tests**

Create `src/groups.rs` containing ONLY this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &[&str])]) -> GroupRules {
        let mut v = Vec::new();
        for (name, paths) in pairs {
            for p in *paths {
                v.push((name.to_string(), PathBuf::from(*p)));
            }
        }
        GroupRules { rules: v }
    }

    #[test]
    fn matches_exact_prefix_and_descendants() {
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(group_for(Path::new("/home/a/dev/alare"), &r), Some("alare"));
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api/src"), &r),
            Some("alare")
        );
    }

    #[test]
    fn respects_path_segment_boundaries() {
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(group_for(Path::new("/home/a/dev/alarehouse"), &r), None);
    }

    #[test]
    fn longest_prefix_wins_regardless_of_order() {
        let r = rules(&[
            ("outer", &["/home/a/dev"]),
            ("inner", &["/home/a/dev/alare"]),
        ]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api"), &r),
            Some("inner")
        );
        assert_eq!(group_for(Path::new("/home/a/dev/other"), &r), Some("outer"));

        // reversed declaration order must give the same answer
        let r2 = rules(&[
            ("inner", &["/home/a/dev/alare"]),
            ("outer", &["/home/a/dev"]),
        ]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api"), &r2),
            Some("inner")
        );
    }

    #[test]
    fn multiple_prefixes_map_to_one_group() {
        let r = rules(&[(
            "alare",
            &["/home/a/dev/alare", "/home/a/.herdr/worktrees/alare"],
        )]);
        assert_eq!(
            group_for(Path::new("/home/a/.herdr/worktrees/alare/issue-1"), &r),
            Some("alare")
        );
    }

    #[test]
    fn trailing_slashes_are_ignored() {
        let r = rules(&[("alare", &["/home/a/dev/alare/"])]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api/"), &r),
            Some("alare")
        );
    }

    #[test]
    fn nonexistent_path_still_resolves() {
        // the daemon must classify an agent whose worktree was deleted
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/deleted-worktree"), &r),
            Some("alare")
        );
    }

    #[test]
    fn no_match_is_none() {
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(group_for(Path::new("/tmp/scratch"), &r), None);
    }

    #[test]
    fn missing_file_is_inactive() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(load(dir.path()), Grouping::Inactive));
    }

    #[test]
    fn valid_file_is_active() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/home/a/dev/alare\"]\n",
        )
        .unwrap();
        match load(dir.path()) {
            Grouping::Active(r) => {
                assert_eq!(r.names(), vec!["alare"]);
                assert_eq!(
                    group_for(Path::new("/home/a/dev/alare/x"), &r),
                    Some("alare")
                );
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[test]
    fn malformed_file_fails_closed_not_inactive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("groups.toml"), "this is not toml {{{").unwrap();
        match load(dir.path()) {
            Grouping::Broken(msg) => assert!(!msg.is_empty()),
            other => panic!("malformed config must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn invalid_group_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\n\"../escape\" = [\"/home/a/dev/x\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }

    #[test]
    fn empty_groups_table_is_broken_not_silently_empty() {
        // an empty table would enroll nobody while looking healthy
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("groups.toml"), "[groups]\n").unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/dev/x"), PathBuf::from(format!("{home}/dev/x")));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1`
Expected: compile errors — `Grouping`, `GroupRules`, `group_for`, `load`, `expand_tilde` not defined.

- [ ] **Step 4: Implement**

Put this ABOVE the test module in `src/groups.rs`:

```rust
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The three config states the spec distinguishes. `Broken` must never be
/// treated as `Inactive`: silently degrading a malformed config into the
/// single shared room would merge two companies' agents, which is the exact
/// outcome this feature exists to prevent.
#[derive(Debug)]
pub enum Grouping {
    Inactive,
    Active(GroupRules),
    Broken(String),
}

#[derive(Debug, Default)]
pub struct GroupRules {
    /// (group name, absolute prefix), one entry per prefix.
    rules: Vec<(String, PathBuf)>,
}

impl GroupRules {
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.rules.iter().map(|(n, _)| n.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn prefixes_for(&self, group: &str) -> Vec<&Path> {
        self.rules
            .iter()
            .filter(|(n, _)| n == group)
            .map(|(_, p)| p.as_path())
            .collect()
    }
}

#[derive(Deserialize)]
struct RawConfig {
    groups: BTreeMap<String, Vec<String>>,
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

fn valid_group_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn load(base: &Path) -> Grouping {
    let path = base.join("groups.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Grouping::Inactive,
        Err(e) => {
            return Grouping::Broken(format!("{} is unreadable: {e}", path.display()));
        }
    };
    let raw: RawConfig = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => return Grouping::Broken(format!("{} is not valid TOML: {e}", path.display())),
    };
    if raw.groups.is_empty() {
        return Grouping::Broken(format!(
            "{} defines no groups; remove the file to disable grouping",
            path.display()
        ));
    }
    let mut rules = Vec::new();
    for (name, prefixes) in raw.groups {
        if !valid_group_name(&name) {
            return Grouping::Broken(format!(
                "{}: invalid group name {name:?} (allowed: [a-z0-9][a-z0-9_-]*)",
                path.display()
            ));
        }
        if prefixes.is_empty() {
            return Grouping::Broken(format!("{}: group {name:?} has no paths", path.display()));
        }
        for p in prefixes {
            rules.push((name.clone(), normalize(&expand_tilde(&p))));
        }
    }
    Grouping::Active(GroupRules { rules })
}

/// Strips trailing separators so `/a/b/` and `/a/b` compare equal.
fn normalize(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().trim_end_matches('/').to_string())
}

/// Longest matching prefix wins — TOML table order is not dependable, so
/// first-match-wins would be nondeterministic, and longest-prefix also lets a
/// nested rule override its parent. Matching is on path-segment boundaries via
/// `Path::starts_with`, so `/dev/alare` never matches `/dev/alarehouse`.
pub fn group_for<'a>(cwd: &Path, rules: &'a GroupRules) -> Option<&'a str> {
    let cwd = normalize(cwd);
    rules
        .rules
        .iter()
        .filter(|(_, prefix)| cwd.starts_with(prefix))
        .max_by_key(|(_, prefix)| prefix.components().count())
        .map(|(name, _)| name.as_str())
}
```

Add `mod groups;` to `src/main.rs` beside the other module declarations.

- [ ] **Step 5: Run tests to verify they pass**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1 && cargo clippy --all-targets`
Expected: all tests pass (13 new), clippy clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/groups.rs src/main.rs
git commit -m "feat: add path-based group config and resolver"
```

---

### Task 2: Group-aware room paths and agent cwd

**Files:**
- Modify: `src/paths.rs` (`room_dir`), `src/herd.rs` (`AgentInfo`, `parse_agent_list`), and every current caller of `room_dir` (`src/main.rs`, `src/cli.rs`, `src/tui.rs`)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `paths::room_dir(group: Option<&str>) -> Result<PathBuf>` — `<base>/<session>` when `None`, `<base>/<session>/<group>` when `Some`, created on demand.
  - `paths::session_dir() -> Result<PathBuf>` — always `<base>/<session>`, for the pidfile and log.
  - `herd::AgentInfo` gains `pub cwd: String`.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/paths.rs`:

```rust
    #[test]
    fn room_dir_appends_group_when_given() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SCUTTLEBUTT_DIR", dir.path());
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/s.sock");
        let ungrouped = room_dir(None).unwrap();
        let grouped = room_dir(Some("alare")).unwrap();
        assert_eq!(grouped, ungrouped.join("alare"));
        assert!(grouped.is_dir());
        std::env::remove_var("SCUTTLEBUTT_DIR");
        std::env::remove_var("HERDR_SOCKET_PATH");
    }

    #[test]
    fn session_dir_ignores_group() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SCUTTLEBUTT_DIR", dir.path());
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/s.sock");
        assert_eq!(session_dir().unwrap(), room_dir(None).unwrap());
        std::env::remove_var("SCUTTLEBUTT_DIR");
        std::env::remove_var("HERDR_SOCKET_PATH");
    }
```

Add to the test module in `src/herd.rs`, and extend the existing `FIXTURE` so its first agent carries a `cwd`:

```rust
    #[test]
    fn parses_agent_cwd() {
        let agents = parse_agent_list(FIXTURE).unwrap();
        assert_eq!(agents[0].cwd, "/home/andy/.herdr/worktrees/alare/issue-590");
    }
```

The existing `FIXTURE`'s first entry must gain `"cwd":"/home/andy/.herdr/worktrees/alare/issue-590",`. Leave the second and third entries without a `cwd` key so the missing-field path stays covered, and add:

```rust
    #[test]
    fn missing_cwd_is_empty_not_an_error() {
        let agents = parse_agent_list(FIXTURE).unwrap();
        assert_eq!(agents[1].cwd, "");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1`
Expected: compile errors — `room_dir` takes no argument, `session_dir` undefined, `AgentInfo` has no `cwd`.

- [ ] **Step 3: Implement**

In `src/paths.rs`, replace `room_dir` with:

```rust
pub fn session_dir() -> Result<PathBuf> {
    let dir = base_dir()?.join(session_key());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The directory holding one room's `room.jsonl` and `state.json`. `None` is
/// the ungrouped layout (grouping inactive) and keeps v1's paths exactly.
pub fn room_dir(group: Option<&str>) -> Result<PathBuf> {
    let dir = match group {
        Some(g) => session_dir()?.join(g),
        None => session_dir()?,
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
```

In `src/herd.rs`, add `pub cwd: String,` to `AgentInfo` and read it in `parse_agent_list`:

```rust
                cwd: a["cwd"].as_str().unwrap_or_default().to_string(),
```

Update every existing `AgentInfo { .. }` literal in test modules across the crate to include `cwd: String::new()` (or a meaningful path where the test cares). Update all current `room_dir()` call sites to `room_dir(None)?` for now — Task 4 gives them real groups. `daemon::status`/`stop`/`run` call sites in `src/main.rs` switch to `paths::session_dir()?` since the pidfile and log are session-level.

- [ ] **Step 4: Run tests to verify they pass**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1 && cargo clippy --all-targets`
Expected: all pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: group-aware room paths and agent cwd"
```

---

### Task 3: Daemon routes agents to per-group rooms

**Files:**
- Modify: `src/daemon.rs` (`run`, plus a new `partition` helper), `src/main.rs`

**Interfaces:**
- Consumes: `groups::{Grouping, GroupRules, group_for, load}`, `paths::{room_dir, session_dir}`, existing `daemon::tick`, `state::{load, save}`
- Produces:
  - `pub fn partition<'a>(agents: &'a [AgentInfo], grouping: &Grouping) -> (Vec<(Option<String>, Vec<AgentInfo>)>, Vec<&'a AgentInfo>)` — returns `(per-group buckets, skipped ungrouped agents)`. With `Grouping::Inactive` there is exactly one bucket keyed `None` holding everyone and no skips. With `Grouping::Broken` there are no buckets and every agent is skipped.
  - `daemon::run(session_dir: &Path, filter: &AgentFilter) -> Result<()>` — signature changes from taking the room dir to taking the session dir.

**`tick` changes by exactly one parameter.** It gains a trailing `group: Option<&str>`, used only to label the introduction; every existing behavior and every existing test is otherwise untouched (existing tests pass `None`). `run` calls it once per group with that group's own state and room dir.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/daemon.rs`:

```rust
    fn agent_at(name: &str, cwd: &str, status: &str) -> AgentInfo {
        AgentInfo {
            name: name.into(),
            pane_id: format!("w1:{name}"),
            status: status.into(),
            cwd: cwd.into(),
        }
    }

    fn two_group_rules() -> crate::groups::Grouping {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/w/alare\"]\nacme = [\"/w/acme\"]\n",
        )
        .unwrap();
        let g = crate::groups::load(dir.path());
        std::mem::forget(dir); // keep the tempdir alive for the test's lifetime
        g
    }

    #[test]
    fn partition_buckets_agents_by_group() {
        let agents = vec![
            agent_at("a1", "/w/alare/api", "idle"),
            agent_at("a2", "/w/acme/web", "idle"),
            agent_at("a3", "/w/alare", "idle"),
        ];
        let (buckets, skipped) = partition(&agents, &two_group_rules());
        assert!(skipped.is_empty());
        let mut names: Vec<(String, Vec<String>)> = buckets
            .into_iter()
            .map(|(g, a)| {
                (
                    g.unwrap(),
                    a.into_iter().map(|x| x.name).collect::<Vec<_>>(),
                )
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ("acme".to_string(), vec!["a2".to_string()]),
                ("alare".to_string(), vec!["a1".to_string(), "a3".to_string()]),
            ]
        );
    }

    #[test]
    fn partition_skips_ungrouped_agents() {
        let agents = vec![
            agent_at("a1", "/w/alare/api", "idle"),
            agent_at("stray", "/tmp/scratch", "idle"),
        ];
        let (buckets, skipped) = partition(&agents, &two_group_rules());
        assert_eq!(buckets.len(), 1);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "stray");
    }

    #[test]
    fn partition_inactive_puts_everyone_in_one_ungrouped_bucket() {
        let agents = vec![
            agent_at("a1", "/w/alare/api", "idle"),
            agent_at("stray", "/tmp/scratch", "idle"),
        ];
        let (buckets, skipped) = partition(&agents, &crate::groups::Grouping::Inactive);
        assert!(skipped.is_empty());
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].0, None);
        assert_eq!(buckets[0].1.len(), 2);
    }

    #[test]
    fn partition_broken_config_enrolls_nobody() {
        let agents = vec![agent_at("a1", "/w/alare/api", "idle")];
        let (buckets, skipped) =
            partition(&agents, &crate::groups::Grouping::Broken("bad".into()));
        assert!(buckets.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn intro_names_the_group_and_forbids_relaying() {
        let members = vec![agent_at("a1", "/w/alare", "idle")];
        let text = intro_text("a1", &members, "scuttlebutt", Some("alare"));
        assert!(text.contains("alare"));
        assert!(text.to_lowercase().contains("relay"));
    }

    #[test]
    fn intro_without_a_group_does_not_mention_one() {
        let members = vec![agent_at("a1", "/w/alare", "idle")];
        let text = intro_text("a1", &members, "scuttlebutt", None);
        assert!(!text.contains("alare"));
    }

    #[test]
    fn agent_moving_between_groups_is_purged_then_reintroduced() {
        // In the old group the agent simply stops appearing, so the existing
        // absence counter must retire it; in the new group it is a fresh
        // enrollment and must be introduced again.
        let old_dir = tempfile::tempdir().unwrap();
        let herd_empty = FakeHerd::new(vec![]);
        let mut old_state = DaemonState::default();
        old_state.cursors.insert("mover".into(), 4);
        old_state.introduced.insert("mover".into());
        for _ in 0..MAX_ABSENCES {
            tick(
                &mut old_state,
                &herd_empty,
                old_dir.path(),
                &AgentFilter::default(),
                Some("alare"),
            )
            .unwrap();
        }
        assert!(!old_state.cursors.contains_key("mover"));
        assert!(!old_state.introduced.contains("mover"));

        let new_dir = tempfile::tempdir().unwrap();
        let herd_new = FakeHerd::new(vec![("mover", "idle")]);
        let mut new_state = DaemonState::default();
        for _ in 0..REQUIRED_SIGHTINGS {
            tick(
                &mut new_state,
                &herd_new,
                new_dir.path(),
                &AgentFilter::default(),
                Some("acme"),
            )
            .unwrap();
        }
        assert!(new_state.introduced.contains("mover"));
        assert!(herd_new.prompts.borrow()[0].1.contains("acme"));
    }

    #[test]
    fn messages_never_cross_group_rooms() {
        let base = tempfile::tempdir().unwrap();
        let alare = base.path().join("alare");
        let acme = base.path().join("acme");
        std::fs::create_dir_all(&alare).unwrap();
        std::fs::create_dir_all(&acme).unwrap();

        let herd_a = FakeHerd::new(vec![("a1", "idle")]);
        let herd_b = FakeHerd::new(vec![("b1", "idle")]);
        let mut state_a = DaemonState::default();
        let mut state_b = DaemonState::default();
        introduced(&mut state_a, &["a1"]);
        introduced(&mut state_b, &["b1"]);
        state_a.cursors.insert("a1".into(), 0);
        state_b.cursors.insert("b1".into(), 0);

        crate::log_store::append(&alare, "human", "alare secret").unwrap();

        tick(&mut state_a, &herd_a, &alare, &AgentFilter::default()).unwrap();
        tick(&mut state_b, &herd_b, &acme, &AgentFilter::default()).unwrap();

        assert!(herd_a.prompts.borrow()[0].1.contains("alare secret"));
        assert!(herd_b.prompts.borrow().is_empty());
        assert_eq!(crate::log_store::read_since(&acme, 0).unwrap().len(), 0);
    }
```

Note: `introduced` and `FakeHerd` already exist in this test module; `FakeHerd::new` must be updated to build `AgentInfo` with a `cwd` field (use `String::new()`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1`
Expected: compile error — `partition` not defined.

- [ ] **Step 3: Implement**

Add to `src/daemon.rs`:

```rust
use crate::groups::{self, Grouping};

/// Splits agents into one bucket per group, plus the agents that belong to no
/// group. `Inactive` yields a single `None` bucket holding everyone (v1
/// behavior). `Broken` yields no buckets at all: a config we cannot parse must
/// never fall back to one shared room, because that would merge groups.
pub fn partition<'a>(
    agents: &'a [AgentInfo],
    grouping: &Grouping,
) -> (Vec<(Option<String>, Vec<AgentInfo>)>, Vec<&'a AgentInfo>) {
    match grouping {
        Grouping::Inactive => (vec![(None, agents.to_vec())], Vec::new()),
        Grouping::Broken(_) => (Vec::new(), agents.iter().collect()),
        Grouping::Active(rules) => {
            let mut buckets: std::collections::BTreeMap<String, Vec<AgentInfo>> =
                std::collections::BTreeMap::new();
            let mut skipped = Vec::new();
            for a in agents {
                match groups::group_for(std::path::Path::new(&a.cwd), rules) {
                    Some(g) => buckets.entry(g.to_string()).or_default().push(a.clone()),
                    None => skipped.push(a),
                }
            }
            (
                buckets.into_iter().map(|(g, a)| (Some(g), a)).collect(),
                skipped,
            )
        }
    }
}
```

Rewrite `run` so it works per group. Replace its body's state/tick handling with:

```rust
pub fn run(session: &Path, filter: &AgentFilter) -> Result<()> {
    if let Some(pid) = read_live_pid(session) {
        report(session, &format!("daemon already running (pid {pid})"));
        anyhow::bail!("daemon already running (pid {pid})");
    }
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))?;
    if let Err(e) = std::fs::write(session.join("daemon.pid"), std::process::id().to_string()) {
        report(session, &format!("could not write pidfile: {e}"));
        return Err(e.into());
    }

    let base = crate::paths::base_dir()?;
    let grouping = groups::load(&base);
    report(session, &format!("room dir {}", session.display()));
    match &grouping {
        Grouping::Inactive => report(session, "grouping inactive (no groups.toml): single room"),
        Grouping::Active(r) => report(
            session,
            &format!("grouping active: groups {}", r.names().join(", ")),
        ),
        Grouping::Broken(msg) => report(
            session,
            &format!("GROUPS CONFIG BROKEN — enrolling nobody: {msg}"),
        ),
    }
    report(session, &format!("agent filter: {}", filter.describe()));

    let herd = crate::herd::RealHerd;
    let mut announced = false;
    while !term.load(Ordering::Relaxed) {
        match herd.list_agents() {
            Ok(all) => {
                let admitted: Vec<AgentInfo> =
                    all.into_iter().filter(|a| filter.admits(&a.name)).collect();
                let (buckets, skipped) = partition(&admitted, &grouping);
                if !announced {
                    for (g, members) in &buckets {
                        let names: Vec<&str> =
                            members.iter().map(|a| a.name.as_str()).collect();
                        report(
                            session,
                            &format!(
                                "enrolling in {}: {}",
                                g.as_deref().unwrap_or("(ungrouped room)"),
                                names.join(", ")
                            ),
                        );
                    }
                    for a in &skipped {
                        report(
                            session,
                            &format!("skipping {} — cwd {} matches no group", a.name, a.cwd),
                        );
                    }
                    announced = true;
                }
                for (group, members) in buckets {
                    let dir = match crate::paths::room_dir(group.as_deref()) {
                        Ok(d) => d,
                        Err(e) => {
                            report(session, &format!("room dir error: {e}"));
                            continue;
                        }
                    };
                    let mut st = crate::state::load(&dir);
                    let scoped = ScopedHerd {
                        inner: &herd,
                        members,
                    };
                    tick_and_save(&mut st, &scoped, &dir);
                }
            }
            Err(e) => report(session, &format!("agent list error: {e}")),
        }
        for _ in 0..20 {
            if term.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    report(session, "daemon stopped");
    let _ = std::fs::remove_file(session.join("daemon.pid"));
    Ok(())
}

/// Presents one group's members to the unchanged `tick`, so `tick` needs no
/// knowledge of grouping: it still sees "the agents" and prompts through the
/// real herd.
struct ScopedHerd<'a> {
    inner: &'a crate::herd::RealHerd,
    members: Vec<AgentInfo>,
}

impl HerdControl for ScopedHerd<'_> {
    fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        Ok(self.members.clone())
    }
    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        self.inner.prompt(name, text)
    }
}
```

`tick_and_save` keeps its existing behavior (save only on `Ok`) but now takes `&dyn HerdControl` and the group; adjust its signature and keep both its tests passing. In `src/main.rs`, `Cmd::Daemon` passes `paths::session_dir()?`.

Give `tick` and `intro_text` a trailing `group: Option<&str>`. `tick` uses it for nothing but the `intro_text` call. In `intro_text`, when the group is `Some(g)`, name it and state the boundary — append a sentence along the lines of:

```rust
    let scope = match group {
        Some(g) => format!(
            " This room is the {g} group: only agents working under {g}'s \
             directories are in it. Do not relay anything from this room into \
             another room, and do not bring other rooms' contents here."
        ),
        None => String::new(),
    };
```

and include `scope` in the returned text. The structural separation is the real control; this sentence is belt-and-braces. Every existing call site in the tests passes `None`, so their assertions are unaffected.

- [ ] **Step 4: Run tests to verify they pass**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1 && cargo clippy --all-targets`
Expected: all pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: route agents to per-group rooms in the daemon"
```

---

### Task 4: Caller group resolution, CLI flags, and `scuttlebutt groups`

**Files:**
- Modify: `src/cli.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `groups::{Grouping, group_for, load}`, `paths::{base_dir, room_dir}`
- Produces:
  - `cli::resolve_group(explicit: Option<&str>, cwd: &Path, grouping: &Grouping) -> Result<Option<String>>` — pure. `Some(name)` when grouping is active, `None` when inactive; error when the cwd is ungrouped or the config is broken, or when an explicit name is not a configured group.
  - `cli::cmd_groups(herd: &dyn HerdControl) -> Result<()>`
  - `cmd_post`, `cmd_read`, `cmd_agents` each take an extra `group: Option<&str>` first argument.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/cli.rs`:

```rust
    use crate::groups::{Grouping, GroupRules};

    fn active() -> Grouping {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/w/alare\"]\nacme = [\"/w/acme\"]\n",
        )
        .unwrap();
        let g = crate::groups::load(dir.path());
        std::mem::forget(dir);
        g
    }

    #[test]
    fn inactive_grouping_resolves_to_no_group() {
        let g = resolve_group(None, Path::new("/anywhere"), &Grouping::Inactive).unwrap();
        assert_eq!(g, None);
    }

    #[test]
    fn active_grouping_resolves_from_cwd() {
        let g = resolve_group(None, Path::new("/w/alare/api"), &active()).unwrap();
        assert_eq!(g.as_deref(), Some("alare"));
    }

    #[test]
    fn explicit_group_overrides_cwd() {
        let g = resolve_group(Some("acme"), Path::new("/w/alare/api"), &active()).unwrap();
        assert_eq!(g.as_deref(), Some("acme"));
    }

    #[test]
    fn unknown_explicit_group_is_an_error() {
        assert!(resolve_group(Some("nope"), Path::new("/w/alare"), &active()).is_err());
    }

    #[test]
    fn ungrouped_cwd_is_refused_not_defaulted() {
        let err = resolve_group(None, Path::new("/tmp/scratch"), &active()).unwrap_err();
        assert!(err.to_string().contains("/tmp/scratch"));
    }

    #[test]
    fn broken_config_is_refused() {
        assert!(
            resolve_group(None, Path::new("/w/alare"), &Grouping::Broken("bad".into())).is_err()
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1`
Expected: compile error — `resolve_group` not defined.

- [ ] **Step 3: Implement**

Add to `src/cli.rs`:

```rust
use crate::groups::{self, Grouping};
use std::path::Path;

/// Which room this invocation talks to. Refusing an ungrouped cwd rather than
/// falling back to a default room is deliberate: a silent fallback would show
/// one group's traffic to a caller the config does not place there.
pub fn resolve_group(
    explicit: Option<&str>,
    cwd: &Path,
    grouping: &Grouping,
) -> Result<Option<String>> {
    match grouping {
        Grouping::Broken(msg) => bail!("groups config is unusable: {msg}"),
        Grouping::Inactive => {
            if let Some(g) = explicit {
                bail!("--group {g} given but no groups.toml exists");
            }
            Ok(None)
        }
        Grouping::Active(rules) => {
            if let Some(g) = explicit {
                if !rules.names().contains(&g) {
                    bail!(
                        "unknown group {g:?}; configured groups: {}",
                        rules.names().join(", ")
                    );
                }
                return Ok(Some(g.to_string()));
            }
            match groups::group_for(cwd, rules) {
                Some(g) => Ok(Some(g.to_string())),
                None => bail!(
                    "cwd {} matches no group; add a prefix for it to groups.toml \
                     or pass --group",
                    cwd.display()
                ),
            }
        }
    }
}

fn current_grouping() -> Result<Grouping> {
    Ok(groups::load(&crate::paths::base_dir()?))
}

/// Resolves the group for a CLI invocation from the process's own cwd.
fn group_for_invocation(explicit: Option<&str>) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    resolve_group(explicit, &cwd, &current_grouping()?)
}

pub fn cmd_groups(herd: &dyn HerdControl) -> Result<()> {
    match current_grouping()? {
        Grouping::Inactive => {
            println!("grouping inactive (no groups.toml) — one room for all agents");
        }
        Grouping::Broken(msg) => {
            println!("groups config BROKEN — no agent is enrolled: {msg}");
        }
        Grouping::Active(rules) => {
            let agents = herd.list_agents().unwrap_or_default();
            for name in rules.names() {
                let paths: Vec<String> = rules
                    .prefixes_for(name)
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                println!("{name}\t{}", paths.join(" "));
                for a in &agents {
                    if groups::group_for(Path::new(&a.cwd), &rules) == Some(name) {
                        println!("  {}\t{}\t{}", a.name, a.status, a.cwd);
                    }
                }
            }
            let ungrouped: Vec<&AgentInfo> = agents
                .iter()
                .filter(|a| groups::group_for(Path::new(&a.cwd), &rules).is_none())
                .collect();
            if !ungrouped.is_empty() {
                println!("ungrouped (receiving nothing)");
                for a in ungrouped {
                    println!("  {}\t{}\t{}", a.name, a.status, a.cwd);
                }
            }
        }
    }
    Ok(())
}
```

Thread the group through the three existing commands: each of `cmd_post`, `cmd_read`, `cmd_agents` gains a leading `group: Option<&str>` parameter, calls `group_for_invocation(group)?`, and passes the result to `paths::room_dir(resolved.as_deref())?`. In `src/main.rs`, add `#[arg(long)] group: Option<String>` to `Post`, `Read`, and `Agents`, add a `/// List groups and their members` `Groups` variant, and wire `Cmd::Groups => cli::cmd_groups(&herd)`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1 && cargo clippy --all-targets`
Expected: all pass, clippy clean.

- [ ] **Step 5: Verify against the live session**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export SCUTTLEBUTT_DIR=/tmp/sb-groups
mkdir -p "$SCUTTLEBUTT_DIR"
cargo run -- groups                                    # expect: grouping inactive
printf '[groups]\nandybarilla = ["%s/dev/andybarilla"]\n' "$HOME" > "$SCUTTLEBUTT_DIR/groups.toml"
cargo run -- groups                                    # expect: andybarilla listed with members
cargo run -- post "hello alare" --as human             # expect: posted #1
cargo run -- read                                      # expect: the message
cd /tmp && cargo run --manifest-path "$OLDPWD/Cargo.toml" -- read 2>&1 | head -2
                                                        # expect: refusal naming /tmp
```

Do NOT start the daemon — the live session has real agents in it.

- [ ] **Step 6: Commit**

```bash
git add src/
git commit -m "feat: group-aware CLI commands and scuttlebutt groups"
```

---

### Task 5: TUI group targeting, title, and display-width wrapping

**Files:**
- Modify: `src/tui.rs`, `src/main.rs`, `Cargo.toml` (add `unicode-width = "0.2"`), `scripts/open-chat.sh`, `scripts/open-chat-tab.sh`

**Interfaces:**
- Consumes: `cli::resolve_group`, `groups::load`, `paths::{base_dir, room_dir}`
- Produces: `tui::run(group: Option<&str>) -> Result<()>`; `tui::wrap_text` measures display width rather than char count.

- [ ] **Step 1: Add the dependency and write the failing test**

In `Cargo.toml` under `[dependencies]`:

```toml
unicode-width = "0.2"
```

`wrap_text` already exists from the previous branch (it currently counts chars). Confirm its exact signature with `grep -n 'fn wrap_text' src/tui.rs` before editing; the tests below assume `fn wrap_text(text: &str, width: usize) -> Vec<String>`. If it differs, keep the existing signature and adapt the tests rather than churning call sites.

Add to the test module in `src/tui.rs`:

```rust
    #[test]
    fn wraps_on_display_width_not_char_count() {
        // each CJK char is two cells wide, so four of them fill an 8-cell row
        let rows = wrap_text("一二三四五六", 8);
        assert_eq!(rows, vec!["一二三四".to_string(), "五六".to_string()]);
    }

    #[test]
    fn wide_text_is_never_truncated() {
        let text = "一二三四五六七八九十";
        let rows = wrap_text(text, 8);
        let rejoined: String = rows.concat();
        assert_eq!(rejoined, text);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1`
Expected: FAIL — the current `wrap_text` counts chars, so it puts eight CJK chars on an 8-cell row.

- [ ] **Step 3: Implement**

In `src/tui.rs`, measure with `unicode_width`:

```rust
use unicode_width::UnicodeWidthChar;

/// Wraps to `width` DISPLAY CELLS. Counting chars would let a CJK or emoji
/// character claim one cell while occupying two, so a row would overflow the
/// pane and be truncated by the renderer with no way to scroll to it.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(c);
        used += w;
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }
    rows
}
```

Change `run` to `pub fn run(group: Option<&str>) -> Result<()>`, resolving the room with:

```rust
    let grouping = crate::groups::load(&crate::paths::base_dir()?);
    let resolved = crate::cli::resolve_group(group, &std::env::current_dir()?, &grouping)?;
    let dir = crate::paths::room_dir(resolved.as_deref())?;
    let title = match resolved.as_deref() {
        Some(g) => format!(" scuttlebutt · {g} "),
        None => " scuttlebutt ".to_string(),
    };
```

and use `title` for the message pane's block title so the group is always visible. Store it on `App` if that is cleaner. In `src/main.rs`, add `#[arg(long)] group: Option<String>` to the `Tui` variant and pass it through.

In both `scripts/open-chat.sh` and `scripts/open-chat-tab.sh`, add `--cwd "$PWD"` to the `herdr plugin pane open` invocation so the pane inherits the invoking directory and resolves the intended group.

- [ ] **Step 4: Run tests to verify they pass**

Run: `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -- --test-threads=1 && cargo clippy --all-targets && cargo build --release`
Expected: all pass, clippy clean, release builds.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/ scripts/
git commit -m "feat: group-targeted TUI with display-width wrapping"
```

---

### Task 6: End-to-end verification

**Files:** none — this task produces evidence.

**Interfaces:** consumes everything.

- [ ] **Step 1: Set up two isolated groups**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export SCUTTLEBUTT_DIR=/tmp/sb-e2e-groups
rm -rf "$SCUTTLEBUTT_DIR" && mkdir -p "$SCUTTLEBUTT_DIR"
mkdir -p /tmp/w/alare /tmp/w/acme
cat > "$SCUTTLEBUTT_DIR/groups.toml" <<'EOF'
[groups]
alare = ["/tmp/w/alare"]
acme  = ["/tmp/w/acme"]
EOF
cargo build --release
```

Create two test agents, one per group directory, named `gossip-a` and `gossip-b` so they are obviously test agents:

```bash
herdr pane split --current --direction right --cwd /tmp/w/alare --no-focus
# take .result.pane.pane_id from the JSON, then:
herdr agent start gossip-a --kind claude --pane <pane-id-1>
herdr pane split --current --direction down --cwd /tmp/w/acme --no-focus
herdr agent start gossip-b --kind claude --pane <pane-id-2>
```

Start the daemon scoped to ONLY the test agents, so the user's real agents are never enrolled:

```bash
SCUTTLEBUTT_AGENTS='gossip-*' ./target/release/scuttlebutt daemon &
```

- [ ] **Step 2: Verify the checklist**

1. `scuttlebutt groups` shows `gossip-a` under `alare` and `gossip-b` under `acme`, and lists no ungrouped test agents.
2. `daemon.log` names both enrollments and their groups.
3. Each agent receives exactly one intro naming its own group.
4. `scuttlebutt post "alare only" --as human --group alare` reaches `gossip-a` and NOT `gossip-b`. This is the property the whole feature exists for — confirm it by reading both agents' panes and by checking that `acme/room.jsonl` never contains the message.
5. A post to `acme` reaches `gossip-b` and not `gossip-a`.
6. `scuttlebutt read --group acme` from a cwd of `/tmp/w/alare` returns acme's messages (explicit override works), while a bare `scuttlebutt read` from `/tmp` is refused naming `/tmp`.
7. Replacing `groups.toml` with malformed content and restarting the daemon enrolls nobody and logs the parse error — it does NOT fall back to one shared room.
8. Removing `groups.toml` and restarting falls back to the single ungrouped room.

- [ ] **Step 3: Clean up**

Stop the daemon, close both panes you opened, and confirm with `herdr agent list` and `herdr pane list` that nothing of yours remains. Do not touch any pane or agent you did not create.

- [ ] **Step 4: Fix anything that failed, then commit**

Each fix reachable by a unit test gets the test first. Then:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test -- --test-threads=1 && cargo clippy --all-targets
git add -A
git commit -m "fix: address groups end-to-end findings"   # only if there were changes
```
