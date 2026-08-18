# Scuttlebutt Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A herdr plugin giving every agent in a herdr session a shared chat room: CLI to post/read, daemon that pushes messages to idle agents, ratatui pane for the human.

**Architecture:** Single Rust binary `scuttlebutt` with subcommands. An append-only `room.jsonl` is the source of truth; all writers append under an advisory `flock`. The daemon polls `herdr agent list`, auto-enrolls named agents, and delivers unseen messages via `herdr agent prompt` only when an agent is `idle`/`done`. The TUI tails the same file. All herdr interaction goes through a `HerdControl` trait so delivery logic is tested against a fake.

**Tech Stack:** Rust (edition 2021). Crates: `clap` 4 (derive), `serde`/`serde_json` 1, `anyhow` 1, `fs2` 0.4 (flock), `chrono` 0.4, `ratatui` 0.29, `crossterm` 0.28, `signal-hook` 0.3.

**Spec:** `docs/superpowers/specs/2026-08-18-scuttlebutt-design.md`

## Global Constraints

- Plugin id: `andybarilla.scuttlebutt`. Binary name: `scuttlebutt`.
- Platforms: linux + macos only. `min_herdr_version = "0.7.0"`.
- Room files live under `<base>/<session-key>/`: `room.jsonl`, `state.json`, `daemon.pid`, `daemon.log`. `<base>` = `$SCUTTLEBUTT_DIR` if set, else `herdr plugin config-dir andybarilla.scuttlebutt` output. `<session-key>` = sanitized `$HERDR_SOCKET_PATH` (else `default`).
- Delivery only when herdr reports `idle` or `done` — never `working`, `blocked`, `unknown`.
- Never deliver an agent's own messages back to it. Batch pending messages into one prompt. 5 consecutive failures for the same batch → skip batch (advance cursor), log loudly.
- Members are agents with a `name` in `herdr agent list`; unnamed agents are not members and cannot post.
- Message JSON: `{"id":u64,"ts":rfc3339,"from":string,"text":string}`, one per line, ids strictly increasing.
- Tests must not touch a live herdr: herdr interaction only via the `HerdControl` trait; file paths via `SCUTTLEBUTT_DIR` + tempdirs.
- No README or other explanatory docs.

---

### Task 1: Cargo scaffold and paths module

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/paths.rs`
- Test: unit tests inline in `src/paths.rs`

**Interfaces:**
- Produces: `paths::session_key() -> String`, `paths::base_dir() -> anyhow::Result<PathBuf>`, `paths::room_dir() -> anyhow::Result<PathBuf>` (base + session key, created on demand).

- [ ] **Step 1: Scaffold the crate**

```toml
# Cargo.toml
[package]
name = "scuttlebutt"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
fs2 = "0.4"
chrono = "0.4"
ratatui = "0.29"
crossterm = "0.28"
signal-hook = "0.3"
```

```rust
// src/main.rs
mod paths;

fn main() {
    println!("scuttlebutt");
}
```

Run: `cargo build` — expect it to fail only because `src/paths.rs` doesn't exist yet; create an empty `src/paths.rs` and confirm `cargo build` succeeds.

- [ ] **Step 2: Write failing tests for paths**

```rust
// src/paths.rs
use anyhow::{Context, Result};
use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_sanitizes_socket_path() {
        std::env::set_var("HERDR_SOCKET_PATH", "/home/andy/.config/herdr/herdr.sock");
        assert_eq!(session_key(), "-home-andy--config-herdr-herdr-sock");
    }

    #[test]
    fn base_dir_prefers_env_override() {
        std::env::set_var("SCUTTLEBUTT_DIR", "/tmp/sb-test");
        assert_eq!(base_dir().unwrap(), PathBuf::from("/tmp/sb-test"));
    }
}
```

Note: these two tests mutate process env; run tests with `--test-threads=1` (put `cargo test -- --test-threads=1` in every test step of this plan) or scope both env vars inside each test before asserting. Use `--test-threads=1`; it is simpler.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile error — `session_key`, `base_dir` not defined.

- [ ] **Step 4: Implement**

```rust
// src/paths.rs (above the tests module)
pub fn session_key() -> String {
    match std::env::var("HERDR_SOCKET_PATH") {
        Ok(p) if !p.is_empty() => p
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect(),
        _ => "default".to_string(),
    }
}

pub fn base_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SCUTTLEBUTT_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let out = std::process::Command::new("herdr")
        .args(["plugin", "config-dir", "andybarilla.scuttlebutt"])
        .output()
        .context("running `herdr plugin config-dir`")?;
    anyhow::ensure!(out.status.success(), "herdr plugin config-dir failed");
    let dir = String::from_utf8(out.stdout)?.trim().to_string();
    anyhow::ensure!(!dir.is_empty(), "empty config dir from herdr");
    Ok(PathBuf::from(dir))
}

pub fn room_dir() -> Result<PathBuf> {
    let dir = base_dir()?.join(session_key());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -- --test-threads=1`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "feat: scaffold scuttlebutt crate with paths module"
```

---

### Task 2: Message log (room.jsonl)

**Files:**
- Create: `src/log_store.rs` (named `log_store`, not `log`, to avoid clashing with the `log` crate ecosystem name)
- Modify: `src/main.rs` (add `mod log_store;`)
- Test: unit tests inline in `src/log_store.rs` using `tempfile`-free tempdirs (`std::env::temp_dir()` + unique suffix) or add `tempfile = "3"` to `[dev-dependencies]` (do this; it's dev-only)

**Interfaces:**
- Produces:
  - `pub struct Message { pub id: u64, pub ts: String, pub from: String, pub text: String }` (derives `Clone, Debug, PartialEq, Serialize, Deserialize`)
  - `log_store::append(dir: &Path, from: &str, text: &str) -> Result<Message>`
  - `log_store::read_since(dir: &Path, since_id: u64) -> Result<Vec<Message>>` — all messages with `id > since_id`, in order
  - `log_store::last_id(dir: &Path) -> Result<u64>` — 0 for missing/empty file

- [ ] **Step 1: Write failing tests**

```rust
// src/log_store.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_assigns_increasing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let m1 = append(dir.path(), "alice", "hello").unwrap();
        let m2 = append(dir.path(), "bob", "hi").unwrap();
        assert_eq!(m1.id, 1);
        assert_eq!(m2.id, 2);
        assert_eq!(m2.from, "bob");
    }

    #[test]
    fn read_since_filters_and_orders() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            append(dir.path(), "alice", &format!("msg{i}")).unwrap();
        }
        let msgs = read_since(dir.path(), 2).unwrap();
        assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn last_id_is_zero_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_id(dir.path()).unwrap(), 0);
    }

    #[test]
    fn torn_trailing_line_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "alice", "ok").unwrap();
        // simulate a torn write
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("room.jsonl"))
            .unwrap();
        write!(f, "{{\"id\":2,\"ts\":\"tr").unwrap();
        drop(f);
        assert_eq!(last_id(dir.path()).unwrap(), 1);
        assert_eq!(read_since(dir.path(), 0).unwrap().len(), 1);
        // next append recovers: id continues from last valid line
        let m = append(dir.path(), "bob", "next").unwrap();
        assert_eq!(m.id, 2);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile errors — `Message`, `append`, `read_since`, `last_id` not defined.

- [ ] **Step 3: Implement**

```rust
// src/log_store.rs (above tests)
use anyhow::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub ts: String,
    pub from: String,
    pub text: String,
}

fn room_file(dir: &Path) -> std::path::PathBuf {
    dir.join("room.jsonl")
}

fn parse_lines(content: &str) -> Vec<Message> {
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<Message>(l).ok())
        .collect()
}

pub fn append(dir: &Path, from: &str, text: &str) -> Result<Message> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(room_file(dir))?;
    file.lock_exclusive()?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let next_id = parse_lines(&content).last().map(|m| m.id).unwrap_or(0) + 1;
    let msg = Message {
        id: next_id,
        ts: chrono::Utc::now().to_rfc3339(),
        from: from.to_string(),
        text: text.to_string(),
    };
    let mut line = serde_json::to_string(&msg)?;
    // Recover from a torn trailing write: if the file doesn't end with \n, prefix one.
    if !content.is_empty() && !content.ends_with('\n') {
        line = format!("\n{line}");
    }
    writeln!(file, "{line}")?;
    fs2::FileExt::unlock(&file)?;
    Ok(msg)
}

pub fn read_since(dir: &Path, since_id: u64) -> Result<Vec<Message>> {
    let content = match std::fs::read_to_string(room_file(dir)) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    Ok(parse_lines(&content)
        .into_iter()
        .filter(|m| m.id > since_id)
        .collect())
}

pub fn last_id(dir: &Path) -> Result<u64> {
    Ok(read_since(dir, 0)?.last().map(|m| m.id).unwrap_or(0))
}
```

Add to `Cargo.toml`:

```toml
[dev-dependencies]
tempfile = "3"
```

Add `mod log_store;` to `src/main.rs`. Note the torn-line test's expectation: the torn fragment has no trailing newline, so `append` writes `\n` + the new line; the fragment plus the prefix newline leaves the torn text on its own line, which `parse_lines` skips. The torn fragment claimed id 2 but never parsed, so id 2 is reused by the next valid append — acceptable per spec ("overwritten by the next locked append" semantics: the torn line is dead data).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -- --test-threads=1`
Expected: PASS (all).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "feat: add locked append-only message log"
```

---

### Task 3: Herd module — herdr CLI wrapper behind a trait

**Files:**
- Create: `src/herd.rs`
- Modify: `src/main.rs` (add `mod herd;`)
- Test: unit tests inline (JSON parsing with a captured fixture)

**Interfaces:**
- Produces:
  - `pub struct AgentInfo { pub name: String, pub pane_id: String, pub status: String }`
  - `pub trait HerdControl { fn list_agents(&self) -> Result<Vec<AgentInfo>>; fn prompt(&self, name: &str, text: &str) -> Result<()>; }`
  - `pub struct RealHerd;` implementing it by shelling out to `herdr`
  - `herd::parse_agent_list(json: &str) -> Result<Vec<AgentInfo>>` (pure, tested)

- [ ] **Step 1: Write failing tests**

The fixture is the real shape of `herdr agent list` (herdr 0.8.0): agents live at `.result.agents[]`; only some entries have `name`.

```rust
// src/herd.rs
#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"id":"cli:agent:list","result":{"agents":[
        {"agent":"claude","agent_status":"idle","name":"issue-590","pane_id":"w35:p1","tab_id":"w35:t1","workspace_id":"w35"},
        {"agent":"claude","agent_status":"working","name":"issue-758","pane_id":"w3A:p1","tab_id":"w3A:t1","workspace_id":"w3A"},
        {"agent":"claude","agent_status":"idle","pane_id":"w3E:p2","tab_id":"w3E:t2","workspace_id":"w3E"}
    ],"type":"agent_list"}}"#;

    #[test]
    fn parses_named_agents_only() {
        let agents = parse_agent_list(FIXTURE).unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "issue-590");
        assert_eq!(agents[0].status, "idle");
        assert_eq!(agents[0].pane_id, "w35:p1");
        assert_eq!(agents[1].name, "issue-758");
        assert_eq!(agents[1].status, "working");
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_agent_list("not json").is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile errors — nothing defined yet.

- [ ] **Step 3: Implement**

```rust
// src/herd.rs (above tests)
use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub name: String,
    pub pane_id: String,
    pub status: String,
}

pub trait HerdControl {
    fn list_agents(&self) -> Result<Vec<AgentInfo>>;
    fn prompt(&self, name: &str, text: &str) -> Result<()>;
}

pub fn parse_agent_list(json: &str) -> Result<Vec<AgentInfo>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing agent list JSON")?;
    let agents = v["result"]["agents"]
        .as_array()
        .context("missing .result.agents")?;
    Ok(agents
        .iter()
        .filter_map(|a| {
            Some(AgentInfo {
                name: a["name"].as_str()?.to_string(),
                pane_id: a["pane_id"].as_str().unwrap_or_default().to_string(),
                status: a["agent_status"].as_str().unwrap_or("unknown").to_string(),
            })
        })
        .collect())
}

pub struct RealHerd;

impl HerdControl for RealHerd {
    fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let out = std::process::Command::new("herdr")
            .args(["agent", "list"])
            .output()
            .context("running `herdr agent list`")?;
        anyhow::ensure!(
            out.status.success(),
            "herdr agent list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_agent_list(&String::from_utf8(out.stdout)?)
    }

    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        let out = std::process::Command::new("herdr")
            .args(["agent", "prompt", name, text])
            .output()
            .context("running `herdr agent prompt`")?;
        anyhow::ensure!(
            out.status.success(),
            "herdr agent prompt {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }
}
```

Note: `prompt` deliberately does NOT pass `--wait` — the daemon must not block on an agent's turn; herdr's own stall detection surfaces as a non-zero exit.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: add HerdControl trait with herdr CLI implementation"
```

---

### Task 4: CLI — post, read, agents

**Files:**
- Create: `src/cli.rs`
- Modify: `src/main.rs` (clap dispatch)
- Test: unit tests inline in `src/cli.rs` for sender resolution and read formatting

**Interfaces:**
- Consumes: `log_store::{append, read_since, last_id, Message}`, `herd::{HerdControl, AgentInfo, RealHerd}`, `paths::room_dir`
- Produces:
  - `cli::resolve_sender(as_flag: Option<&str>, pane_env: Option<&str>, agents: &[AgentInfo]) -> Result<String>`
  - `cli::format_messages(msgs: &[Message]) -> String` — lines of `[#id ts] from: text`
  - subcommands wired in `main.rs`: `post`, `read`, `agents`

- [ ] **Step 1: Write failing tests**

```rust
// src/cli.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::herd::AgentInfo;
    use crate::log_store::Message;

    fn agents() -> Vec<AgentInfo> {
        vec![AgentInfo {
            name: "reviewer".into(),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
        }]
    }

    #[test]
    fn as_flag_wins() {
        assert_eq!(
            resolve_sender(Some("human"), Some("w1:p1"), &agents()).unwrap(),
            "human"
        );
    }

    #[test]
    fn pane_env_resolves_to_agent_name() {
        assert_eq!(
            resolve_sender(None, Some("w1:p1"), &agents()).unwrap(),
            "reviewer"
        );
    }

    #[test]
    fn unknown_pane_is_an_error() {
        assert!(resolve_sender(None, Some("w9:p9"), &agents()).is_err());
        assert!(resolve_sender(None, None, &agents()).is_err());
    }

    #[test]
    fn formats_messages() {
        let msgs = vec![Message {
            id: 3,
            ts: "2026-08-18T12:00:00+00:00".into(),
            from: "reviewer".into(),
            text: "done".into(),
        }];
        assert_eq!(
            format_messages(&msgs),
            "[#3 2026-08-18T12:00:00+00:00] reviewer: done\n"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile errors.

- [ ] **Step 3: Implement cli.rs and main.rs dispatch**

```rust
// src/cli.rs (above tests)
use crate::herd::{AgentInfo, HerdControl};
use crate::log_store::{self, Message};
use anyhow::{bail, Result};

pub fn resolve_sender(
    as_flag: Option<&str>,
    pane_env: Option<&str>,
    agents: &[AgentInfo],
) -> Result<String> {
    if let Some(name) = as_flag {
        return Ok(name.to_string());
    }
    if let Some(pane) = pane_env {
        if let Some(a) = agents.iter().find(|a| a.pane_id == pane) {
            return Ok(a.name.clone());
        }
    }
    bail!(
        "cannot determine sender: pass --as <name>, or run from a pane \
         hosting a named herdr agent"
    );
}

pub fn format_messages(msgs: &[Message]) -> String {
    msgs.iter()
        .map(|m| format!("[#{} {}] {}: {}\n", m.id, m.ts, m.from, m.text))
        .collect()
}

pub fn cmd_post(herd: &dyn HerdControl, as_flag: Option<&str>, text: &str) -> Result<()> {
    let dir = crate::paths::room_dir()?;
    let pane = std::env::var("HERDR_PANE_ID").ok();
    let sender = resolve_sender(as_flag, pane.as_deref(), &herd.list_agents()?)?;
    let msg = log_store::append(&dir, &sender, text)?;
    println!("posted #{}", msg.id);
    Ok(())
}

pub fn cmd_read(since: Option<u64>, limit: usize) -> Result<()> {
    let dir = crate::paths::room_dir()?;
    let mut msgs = log_store::read_since(&dir, since.unwrap_or(0))?;
    if since.is_none() && msgs.len() > limit {
        msgs = msgs.split_off(msgs.len() - limit);
    }
    print!("{}", format_messages(&msgs));
    Ok(())
}

pub fn cmd_agents(herd: &dyn HerdControl) -> Result<()> {
    for a in herd.list_agents()? {
        println!("{}\t{}\t{}", a.name, a.status, a.pane_id);
    }
    println!("human\t-\t-");
    Ok(())
}
```

```rust
// src/main.rs
mod cli;
mod herd;
mod log_store;
mod paths;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "scuttlebutt", about = "Chat room for herdr agents")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Post a message to the room
    Post {
        text: String,
        /// Post as this name instead of resolving from the calling pane
        #[arg(long = "as")]
        as_name: Option<String>,
    },
    /// Print room messages
    Read {
        /// Only messages with id greater than this
        #[arg(long)]
        since: Option<u64>,
        /// Max messages when --since is not given
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List room members
    Agents,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let herd = herd::RealHerd;
    match args.cmd {
        Cmd::Post { text, as_name } => cli::cmd_post(&herd, as_name.as_deref(), &text),
        Cmd::Read { since, limit } => cli::cmd_read(since, limit),
        Cmd::Agents => cli::cmd_agents(&herd),
    }
}
```

- [ ] **Step 4: Run tests, then smoke-test against live herdr**

Run: `cargo test -- --test-threads=1` — expect PASS.
Then (inside a herdr pane, with an isolated dir):

```bash
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- post "hello room" --as human
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- read
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- agents
```

Expected: `posted #1`, the formatted message, and the live agent list plus `human`.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: add post/read/agents CLI commands"
```

---

### Task 5: Daemon state persistence

**Files:**
- Create: `src/state.rs`
- Modify: `src/main.rs` (add `mod state;`)
- Test: unit tests inline

**Interfaces:**
- Produces:
  - `pub struct DaemonState { pub cursors: HashMap<String, u64>, pub introduced: HashSet<String>, pub fail_counts: HashMap<String, u32> }` (derives `Default, Serialize, Deserialize`)
  - `state::load(dir: &Path) -> DaemonState` — missing/corrupt file → `Default`
  - `state::save(dir: &Path, s: &DaemonState) -> Result<()>` — atomic (write `state.json.tmp`, rename)

- [ ] **Step 1: Write failing tests**

```rust
// src/state.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = DaemonState::default();
        s.cursors.insert("reviewer".into(), 7);
        s.introduced.insert("reviewer".into());
        save(dir.path(), &s).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.cursors["reviewer"], 7);
        assert!(loaded.introduced.contains("reviewer"));
    }

    #[test]
    fn missing_or_corrupt_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).cursors.is_empty());
        std::fs::write(dir.path().join("state.json"), "garbage").unwrap();
        assert!(load(dir.path()).cursors.is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
// src/state.rs (above tests)
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DaemonState {
    pub cursors: HashMap<String, u64>,
    pub introduced: HashSet<String>,
    pub fail_counts: HashMap<String, u32>,
}

pub fn load(dir: &Path) -> DaemonState {
    std::fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, s: &DaemonState) -> Result<()> {
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(tmp, dir.join("state.json"))?;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: add daemon state persistence"
```

---

### Task 6: Delivery tick logic

**Files:**
- Create: `src/daemon.rs`
- Modify: `src/main.rs` (add `mod daemon;`)
- Test: unit tests inline against a `FakeHerd`

**Interfaces:**
- Consumes: `herd::{HerdControl, AgentInfo}`, `log_store::{read_since, last_id, Message}`, `state::DaemonState`
- Produces:
  - `daemon::tick(state: &mut DaemonState, herd: &dyn HerdControl, dir: &Path) -> Result<()>` — one full poll/deliver pass; all rules from Global Constraints
  - `daemon::intro_text(name: &str, members: &[AgentInfo], exe: &str) -> String`
  - `const MAX_BATCH_FAILURES: u32 = 5;`

- [ ] **Step 1: Write failing tests**

```rust
// src/daemon.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::herd::AgentInfo;
    use crate::log_store::append;
    use std::cell::RefCell;

    struct FakeHerd {
        agents: Vec<AgentInfo>,
        prompts: RefCell<Vec<(String, String)>>,
        fail_prompts: bool,
    }

    impl FakeHerd {
        fn new(agents: Vec<(&str, &str)>) -> Self {
            FakeHerd {
                agents: agents
                    .into_iter()
                    .map(|(n, s)| AgentInfo {
                        name: n.into(),
                        pane_id: format!("w1:{n}"),
                        status: s.into(),
                    })
                    .collect(),
                prompts: RefCell::new(vec![]),
                fail_prompts: false,
            }
        }
    }

    impl HerdControl for FakeHerd {
        fn list_agents(&self) -> anyhow::Result<Vec<AgentInfo>> {
            Ok(self.agents.clone())
        }
        fn prompt(&self, name: &str, text: &str) -> anyhow::Result<()> {
            if self.fail_prompts {
                anyhow::bail!("stalled");
            }
            self.prompts.borrow_mut().push((name.into(), text.into()));
            Ok(())
        }
    }

    fn introduced(state: &mut DaemonState, names: &[&str]) {
        for n in names {
            state.introduced.insert(n.to_string());
        }
    }

    #[test]
    fn new_agent_gets_intro_and_no_history() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "human", "old message").unwrap();
        let herd = FakeHerd::new(vec![("reviewer", "idle")]);
        let mut state = DaemonState::default();
        tick(&mut state, &herd, dir.path()).unwrap();
        let prompts = herd.prompts.borrow();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].0, "reviewer");
        assert!(prompts[0].1.contains("post"));
        // cursor starts at tail: the old message is never delivered
        assert_eq!(state.cursors["reviewer"], 1);
    }

    #[test]
    fn delivers_batched_messages_to_idle_only() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![("reviewer", "idle"), ("builder", "working")]);
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer", "builder"]);
        state.cursors.insert("reviewer".into(), 0);
        state.cursors.insert("builder".into(), 0);
        append(dir.path(), "human", "one").unwrap();
        append(dir.path(), "human", "two").unwrap();
        tick(&mut state, &herd, dir.path()).unwrap();
        let prompts = herd.prompts.borrow();
        assert_eq!(prompts.len(), 1); // builder is working: nothing
        assert_eq!(prompts[0].0, "reviewer");
        assert!(prompts[0].1.contains("one") && prompts[0].1.contains("two"));
        assert_eq!(state.cursors["reviewer"], 2);
        assert_eq!(state.cursors["builder"], 0);
    }

    #[test]
    fn never_delivers_own_messages() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![("reviewer", "idle")]);
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer"]);
        state.cursors.insert("reviewer".into(), 0);
        append(dir.path(), "reviewer", "my own post").unwrap();
        tick(&mut state, &herd, dir.path()).unwrap();
        assert!(herd.prompts.borrow().is_empty());
        // cursor still advances past own messages
        assert_eq!(state.cursors["reviewer"], 1);
    }

    #[test]
    fn vanished_agent_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![]);
        let mut state = DaemonState::default();
        state.cursors.insert("ghost".into(), 3);
        state.introduced.insert("ghost".into());
        tick(&mut state, &herd, dir.path()).unwrap();
        assert!(state.cursors.is_empty());
        assert!(state.introduced.is_empty());
    }

    #[test]
    fn failed_batch_retries_then_skips() {
        let dir = tempfile::tempdir().unwrap();
        let mut herd = FakeHerd::new(vec![("reviewer", "idle")]);
        herd.fail_prompts = true;
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer"]);
        state.cursors.insert("reviewer".into(), 0);
        append(dir.path(), "human", "hello").unwrap();
        for _ in 0..MAX_BATCH_FAILURES {
            tick(&mut state, &herd, dir.path()).unwrap();
        }
        // after 5 consecutive failures the batch is skipped
        assert_eq!(state.cursors["reviewer"], 1);
        assert_eq!(state.fail_counts.get("reviewer"), None);
    }

    #[test]
    fn success_resets_fail_count() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![("reviewer", "idle")]);
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer"]);
        state.cursors.insert("reviewer".into(), 0);
        state.fail_counts.insert("reviewer".into(), 3);
        append(dir.path(), "human", "hello").unwrap();
        tick(&mut state, &herd, dir.path()).unwrap();
        assert_eq!(state.cursors["reviewer"], 1);
        assert_eq!(state.fail_counts.get("reviewer"), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
// src/daemon.rs (above tests)
use crate::herd::{AgentInfo, HerdControl};
use crate::log_store;
use crate::state::DaemonState;
use anyhow::Result;
use std::path::Path;

pub const MAX_BATCH_FAILURES: u32 = 5;

pub fn intro_text(name: &str, members: &[AgentInfo], exe: &str) -> String {
    let others: Vec<&str> = members
        .iter()
        .map(|a| a.name.as_str())
        .filter(|n| *n != name)
        .collect();
    let others = if others.is_empty() {
        "none yet".to_string()
    } else {
        others.join(", ")
    };
    format!(
        "[scuttlebutt] You are in this herdr session's shared chat room. \
         Other members: {others} (plus the human).\n\
         To post: {exe} post \"your message\"\n\
         To catch up: {exe} read\n\
         New messages from others are delivered to you automatically when \
         you are idle; a message you already saw via `read` may be delivered \
         again. Keep messages short and purposeful. No action needed now."
    )
}

fn deliverable(status: &str) -> bool {
    status == "idle" || status == "done"
}

pub fn tick(state: &mut DaemonState, herd: &dyn HerdControl, dir: &Path) -> Result<()> {
    let agents = herd.list_agents()?;
    let tail = log_store::last_id(dir)?;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "scuttlebutt".to_string());

    // enroll new, drop vanished
    for a in &agents {
        state.cursors.entry(a.name.clone()).or_insert(tail);
    }
    let live: std::collections::HashSet<String> =
        agents.iter().map(|a| a.name.clone()).collect();
    state.cursors.retain(|k, _| live.contains(k));
    state.introduced.retain(|k| live.contains(k));
    state.fail_counts.retain(|k, _| live.contains(k));

    for a in &agents {
        if !deliverable(&a.status) {
            continue;
        }
        if !state.introduced.contains(&a.name) {
            match herd.prompt(&a.name, &intro_text(&a.name, &agents, &exe)) {
                Ok(()) => {
                    state.introduced.insert(a.name.clone());
                }
                Err(e) => {
                    eprintln!("[scuttlebutt] intro to {} failed: {e}", a.name);
                    continue;
                }
            }
        }
        let cursor = state.cursors[&a.name];
        let pending = log_store::read_since(dir, cursor)?;
        let Some(max_id) = pending.last().map(|m| m.id) else {
            continue;
        };
        let others: Vec<_> = pending.iter().filter(|m| m.from != a.name).collect();
        if others.is_empty() {
            state.cursors.insert(a.name.clone(), max_id);
            continue;
        }
        let body: String = others
            .iter()
            .map(|m| format!("[#{}] {}: {}\n", m.id, m.from, m.text))
            .collect();
        let text = format!("[scuttlebutt] New messages in the room:\n{body}");
        match herd.prompt(&a.name, &text) {
            Ok(()) => {
                state.cursors.insert(a.name.clone(), max_id);
                state.fail_counts.remove(&a.name);
            }
            Err(e) => {
                let fails = state.fail_counts.entry(a.name.clone()).or_insert(0);
                *fails += 1;
                eprintln!(
                    "[scuttlebutt] delivery to {} failed ({}/{MAX_BATCH_FAILURES}): {e}",
                    a.name, fails
                );
                if *fails >= MAX_BATCH_FAILURES {
                    eprintln!(
                        "[scuttlebutt] SKIPPING batch up to #{max_id} for {} after \
                         {MAX_BATCH_FAILURES} failures",
                        a.name
                    );
                    state.cursors.insert(a.name.clone(), max_id);
                    state.fail_counts.remove(&a.name);
                }
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -- --test-threads=1`
Expected: PASS (all 6 new tests).

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: add daemon delivery tick logic"
```

---

### Task 7: Daemon runner — pidfile, loop, signals, status/stop

**Files:**
- Modify: `src/daemon.rs` (add `run`, `status`, `stop`), `src/main.rs` (subcommands `daemon`, `daemon-status`, `daemon-stop`)
- Test: pidfile helpers unit-tested; the loop is exercised in the manual E2E (Task 10)

**Interfaces:**
- Consumes: `daemon::tick`, `state::{load, save}`, `paths::room_dir`
- Produces:
  - `daemon::run(dir: &Path) -> Result<()>` — foreground loop, 2s tick, SIGTERM/SIGINT clean exit, appends to `daemon.log`
  - `daemon::read_live_pid(dir: &Path) -> Option<u32>` — pid from `daemon.pid` if that process is alive
  - `daemon::status(dir: &Path)` / `daemon::stop(dir: &Path) -> Result<()>`

- [ ] **Step 1: Write failing tests for pidfile helpers**

```rust
// append to src/daemon.rs tests module
#[test]
fn read_live_pid_detects_own_process() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_live_pid(dir.path()), None);
    std::fs::write(dir.path().join("daemon.pid"), std::process::id().to_string()).unwrap();
    assert_eq!(read_live_pid(dir.path()), Some(std::process::id()));
}

#[test]
fn read_live_pid_ignores_stale_pid() {
    let dir = tempfile::tempdir().unwrap();
    // pid 4194304 is above the default linux pid_max; nothing alive there
    std::fs::write(dir.path().join("daemon.pid"), "4194304").unwrap();
    assert_eq!(read_live_pid(dir.path()), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile errors — `read_live_pid` not defined.

- [ ] **Step 3: Implement**

```rust
// src/daemon.rs (add)
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn pid_alive(pid: u32) -> bool {
    // signal 0: existence check
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

// tiny extern to avoid pulling in the libc crate for one call
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

pub fn read_live_pid(dir: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(dir.join("daemon.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    pid_alive(pid).then_some(pid)
}

fn log_line(dir: &Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
    {
        let _ = writeln!(f, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}

pub fn run(dir: &Path) -> Result<()> {
    if let Some(pid) = read_live_pid(dir) {
        anyhow::bail!("daemon already running (pid {pid})");
    }
    std::fs::write(dir.join("daemon.pid"), std::process::id().to_string())?;
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))?;
    log_line(dir, "daemon started");
    let herd = crate::herd::RealHerd;
    let mut state = crate::state::load(dir);
    while !term.load(Ordering::Relaxed) {
        if let Err(e) = tick(&mut state, &herd, dir) {
            log_line(dir, &format!("tick error: {e}"));
        }
        if let Err(e) = crate::state::save(dir, &state) {
            log_line(dir, &format!("state save error: {e}"));
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    log_line(dir, "daemon stopped");
    let _ = std::fs::remove_file(dir.join("daemon.pid"));
    Ok(())
}

pub fn status(dir: &Path) {
    match read_live_pid(dir) {
        Some(pid) => println!("running (pid {pid})"),
        None => println!("not running"),
    }
}

pub fn stop(dir: &Path) -> Result<()> {
    match read_live_pid(dir) {
        Some(pid) => {
            unsafe { libc_kill(pid as i32, 15) }; // SIGTERM
            println!("sent SIGTERM to pid {pid}");
            Ok(())
        }
        None => {
            println!("not running");
            Ok(())
        }
    }
}
```

Also route `eprintln!` calls in `tick` through daemon.log: replace the three `eprintln!` calls in `tick` with a small `fn report(dir: &Path, line: &str)` that calls both `eprintln!` and `log_line` (define `report` next to `log_line`; pass `dir` — `tick` already has it).

Add to `main.rs`:

```rust
// in enum Cmd:
    /// Run the delivery daemon in the foreground
    Daemon,
    /// Show daemon status
    DaemonStatus,
    /// Stop the daemon
    DaemonStop,

// in match:
    Cmd::Daemon => daemon::run(&paths::room_dir()?),
    Cmd::DaemonStatus => {
        daemon::status(&paths::room_dir()?);
        Ok(())
    }
    Cmd::DaemonStop => daemon::stop(&paths::room_dir()?),
```

(clap derives kebab-case: `daemon-status`, `daemon-stop`. Add `mod daemon;` if not present.)

- [ ] **Step 4: Run tests, smoke-test the runner**

Run: `cargo test -- --test-threads=1` — expect PASS.

```bash
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- daemon &   # backgrounded foreground run
sleep 3
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- daemon-status   # expect: running (pid ...)
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- daemon-stop
sleep 3
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- daemon-status   # expect: not running
cat /tmp/sb-smoke/*/daemon.log                              # started/stopped lines
```

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: add daemon runner with pidfile and signal handling"
```

---

### Task 8: TUI chat pane

**Files:**
- Create: `src/tui.rs`
- Modify: `src/main.rs` (subcommand `tui`)
- Test: unit tests for the pure app-state update; rendering verified manually

**Interfaces:**
- Consumes: `log_store::{append, read_since, Message}`, `herd::{HerdControl, RealHerd, AgentInfo}`, `paths::room_dir`
- Produces:
  - `pub struct App { pub messages: Vec<Message>, pub input: String, pub members: Vec<AgentInfo>, pub scroll_from_bottom: u16, pub quit: bool }`
  - `tui::handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<String>` — pure; returns `Some(text)` when Enter submits non-empty input
  - `tui::run() -> Result<()>`

- [ ] **Step 1: Write failing tests for handle_key**

```rust
// src/tui.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn app() -> App {
        App::default()
    }

    #[test]
    fn typing_appends_to_input() {
        let mut a = app();
        handle_key(&mut a, KeyCode::Char('h'), KeyModifiers::NONE);
        handle_key(&mut a, KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(a.input, "hi");
    }

    #[test]
    fn enter_submits_and_clears() {
        let mut a = app();
        a.input = "hello".into();
        let submitted = handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(submitted.as_deref(), Some("hello"));
        assert_eq!(a.input, "");
    }

    #[test]
    fn enter_on_empty_input_is_noop() {
        let mut a = app();
        assert_eq!(handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE), None);
    }

    #[test]
    fn ctrl_c_and_esc_quit() {
        let mut a = app();
        handle_key(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.quit);
        let mut b = app();
        handle_key(&mut b, KeyCode::Esc, KeyModifiers::NONE);
        assert!(b.quit);
    }

    #[test]
    fn backspace_deletes() {
        let mut a = app();
        a.input = "hi".into();
        handle_key(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(a.input, "h");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --test-threads=1`
Expected: compile errors.

- [ ] **Step 3: Implement App, handle_key, and run**

```rust
// src/tui.rs (above tests)
use crate::herd::{AgentInfo, HerdControl, RealHerd};
use crate::log_store::{self, Message};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

#[derive(Default)]
pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub members: Vec<AgentInfo>,
    pub scroll_from_bottom: u16,
    pub quit: bool,
}

pub fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
            app.quit = true;
            None
        }
        (KeyCode::Enter, _) => {
            if app.input.trim().is_empty() {
                None
            } else {
                let text = std::mem::take(&mut app.input);
                app.scroll_from_bottom = 0;
                Some(text)
            }
        }
        (KeyCode::Backspace, _) => {
            app.input.pop();
            None
        }
        (KeyCode::Up, _) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(1);
            None
        }
        (KeyCode::Down, _) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(1);
            None
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            app.input.push(c);
            None
        }
        _ => None,
    }
}

pub fn run() -> Result<()> {
    let dir = crate::paths::room_dir()?;
    let herd = RealHerd;
    let mut app = App::default();
    app.messages = log_store::read_since(&dir, 0)?;
    app.members = herd.list_agents().unwrap_or_default();

    let mut terminal = ratatui::init();
    let mut last_member_refresh = std::time::Instant::now();
    let result = (|| -> Result<()> {
        while !app.quit {
            // tail new messages every loop; members on a slow tick
            let last = app.messages.last().map(|m| m.id).unwrap_or(0);
            let mut fresh = log_store::read_since(&dir, last)?;
            app.messages.append(&mut fresh);
            if last_member_refresh.elapsed() > std::time::Duration::from_secs(3) {
                if let Ok(m) = herd.list_agents() {
                    app.members = m;
                }
                last_member_refresh = std::time::Instant::now();
            }

            terminal.draw(|f| draw(f, &app))?;

            if event::poll(std::time::Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        if let Some(text) = handle_key(&mut app, key.code, key.modifiers) {
                            log_store::append(&dir, "human", &text)?;
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(f.area());
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(24)])
        .split(outer[0]);

    let lines: Vec<Line> = app
        .messages
        .iter()
        .map(|m| {
            let who_style = if m.from == "human" {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(format!("{}: ", m.from), who_style),
                Span::raw(m.text.clone()),
            ])
        })
        .collect();
    let total = lines.len() as u16;
    let visible = top[0].height.saturating_sub(2);
    let bottom_offset = total.saturating_sub(visible);
    let scroll = bottom_offset.saturating_sub(app.scroll_from_bottom.min(bottom_offset));
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" scuttlebutt "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        top[0],
    );

    let members: Vec<ListItem> = app
        .members
        .iter()
        .map(|a| {
            let color = match a.status.as_str() {
                "idle" | "done" => Color::Green,
                "working" => Color::Blue,
                "blocked" => Color::Red,
                _ => Color::DarkGray,
            };
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::default().fg(color)),
                Span::raw(a.name.clone()),
            ]))
        })
        .chain(std::iter::once(ListItem::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(Color::Yellow)),
            Span::raw("human (you)"),
        ]))))
        .collect();
    f.render_widget(
        List::new(members).block(Block::default().borders(Borders::ALL).title(" members ")),
        top[1],
    );

    f.render_widget(
        Paragraph::new(app.input.as_str())
            .block(Block::default().borders(Borders::ALL).title(" message (Enter to send, Esc to quit) ")),
        outer[1],
    );
}
```

Add to `main.rs`: `mod tui;`, `Cmd::Tui` variant (`/// Open the chat TUI`), arm `Cmd::Tui => tui::run(),`.

- [ ] **Step 4: Run tests, then eyeball the TUI**

Run: `cargo test -- --test-threads=1` — expect PASS.
Then in a spare terminal inside herdr:

```bash
SCUTTLEBUTT_DIR=/tmp/sb-smoke cargo run -- tui
```

Verify: message list shows earlier smoke messages, member sidebar lists live agents with colored dots, typing + Enter posts as `human` and appears in the list, Up/Down scrolls, Esc quits and restores the terminal.

- [ ] **Step 5: Commit**

```bash
git add src/
git commit -m "feat: add ratatui chat pane"
```

---

### Task 9: Plugin packaging — manifest and launcher scripts

**Files:**
- Create: `herdr-plugin.toml`, `scripts/open-chat.sh`, `scripts/open-chat-tab.sh`, `scripts/daemon-ctl.sh`, `.gitignore`

**Interfaces:**
- Consumes: the `scuttlebutt` binary at `target/release/scuttlebutt` with subcommands `tui`, `daemon`, `daemon-status`, `daemon-stop` (Tasks 7–8)
- Produces: installable herdr plugin `andybarilla.scuttlebutt` with actions `open-chat`, `open-chat-tab`, `daemon-start`, `daemon-stop`, `daemon-status` and pane entrypoint `chat`

- [ ] **Step 1: Write the manifest**

```toml
# herdr-plugin.toml
id = "andybarilla.scuttlebutt"
name = "Scuttlebutt"
version = "0.1.0"
min_herdr_version = "0.7.0"
description = "Shared chat room for the agents in a herdr session"
platforms = ["linux", "macos"]

[[build]]
command = ["cargo", "build", "--release"]
platforms = ["linux", "macos"]

[[panes]]
id = "chat"
title = "Scuttlebutt"
placement = "split"
command = ["./target/release/scuttlebutt", "tui"]

[[actions]]
id = "open-chat"
title = "Open chat"
description = "Open the agent chat room in a split pane (starts the daemon if needed)."
command = ["bash", "scripts/open-chat.sh"]

[[actions]]
id = "open-chat-tab"
title = "Open chat (tab)"
description = "Open the agent chat room in its own tab (starts the daemon if needed)."
command = ["bash", "scripts/open-chat-tab.sh"]

[[actions]]
id = "daemon-start"
title = "Start delivery daemon"
command = ["bash", "scripts/daemon-ctl.sh", "start"]

[[actions]]
id = "daemon-stop"
title = "Stop delivery daemon"
command = ["bash", "scripts/daemon-ctl.sh", "stop"]

[[actions]]
id = "daemon-status"
title = "Daemon status"
command = ["bash", "scripts/daemon-ctl.sh", "status"]
```

- [ ] **Step 2: Write the scripts**

```bash
#!/usr/bin/env bash
# scripts/daemon-ctl.sh — start/stop/status for the delivery daemon.
# The binary derives all paths (room dir, pidfile) itself; this script only
# handles detaching on start.
set -euo pipefail
cmd="${1:?usage: daemon-ctl.sh start|stop|status}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
bin="$script_dir/../target/release/scuttlebutt"
[ -x "$bin" ] || { echo "scuttlebutt not built; run: cargo build --release" >&2; exit 1; }

case "$cmd" in
  start)
    if "$bin" daemon-status | grep -q '^running'; then
      "$bin" daemon-status
      exit 0
    fi
    nohup "$bin" daemon >/dev/null 2>&1 &
    disown
    sleep 1
    "$bin" daemon-status
    ;;
  stop)   exec "$bin" daemon-stop ;;
  status) exec "$bin" daemon-status ;;
  *) echo "unknown command: $cmd" >&2; exit 1 ;;
esac
```

```bash
#!/usr/bin/env bash
# scripts/open-chat.sh — start daemon if needed, open the chat pane as a split.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
bash "$script_dir/daemon-ctl.sh" start >/dev/null
exec "$herdr_bin" plugin pane open \
  --plugin andybarilla.scuttlebutt \
  --entrypoint chat \
  --placement split \
  --direction right \
  --focus
```

```bash
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
  --focus
```

```gitignore
# .gitignore
target/
```

Run: `chmod +x scripts/*.sh`

- [ ] **Step 3: Link the plugin and verify actions load**

```bash
cargo build --release
herdr plugin link /home/andy/dev/andybarilla/herdr-scuttlebutt
herdr plugin list | grep -A2 scuttlebutt
herdr plugin action list | grep scuttlebutt
```

Expected: plugin listed as linked and enabled; five actions present. If `plugin link` rejects the manifest, read the error — field names above match herdr-file-viewer 1.15.0 / herdr 0.8.0; adjust to what the installed herdr reports and note the difference in the commit message.

- [ ] **Step 4: Invoke the actions**

```bash
herdr plugin action invoke andybarilla.scuttlebutt daemon-start
herdr plugin action invoke andybarilla.scuttlebutt daemon-status
herdr plugin action invoke andybarilla.scuttlebutt open-chat
```

(If `action invoke` syntax differs, run `herdr plugin action` for usage.)
Expected: daemon reports running; a Scuttlebutt pane opens with the TUI.

- [ ] **Step 5: Commit**

```bash
git add herdr-plugin.toml scripts/ .gitignore
git commit -m "feat: add herdr plugin manifest and launcher scripts"
```

---

### Task 10: End-to-end verification

**Files:** none (manual verification against the live herdr session)

**Interfaces:**
- Consumes: everything.

- [ ] **Step 1: Set up two test agents**

In the herdr session, create two panes and start cheap named agents in them (any supported kind that is installed; check `herdr agent` for kinds):

```bash
herdr pane split --current --direction right --cwd "$PWD" --no-focus
# note pane id from .result.pane.pane_id, then:
herdr agent start gossip-a --kind claude --pane <pane-id-1>
herdr pane split --current --direction down --cwd "$PWD" --no-focus
herdr agent start gossip-b --kind claude --pane <pane-id-2>
```

- [ ] **Step 2: Verify the checklist**

With the daemon running and the TUI open:

1. Both agents receive one intro prompt each (watch their panes), and only one — restart the daemon (`daemon-stop`, `daemon-start`) and confirm no second intro (state.json persists `introduced`).
2. Post from the TUI as human → both agents receive it when idle; the TUI shows it immediately.
3. Ask gossip-a (via `herdr agent prompt gossip-a "Post 'hello from a' to the chat room"`) → message appears in TUI, gossip-b receives it, gossip-a does NOT receive its own message back.
4. While gossip-b is `working`, post from the TUI → nothing is injected into gossip-b until it goes idle, then the batch arrives as one prompt.
5. Kill the daemon mid-flight (`kill -9 <pid>`), post a message, restart via action → the message is delivered (log is truth; state cursor was behind).
6. Close gossip-a's pane → `scuttlebutt agents` and the TUI member list drop it within a few seconds.
7. `tail -f` the room file works: `tail -f "$(SCUTTLEBUTT_DIR= herdr plugin config-dir andybarilla.scuttlebutt)"/*/room.jsonl`.

- [ ] **Step 3: Fix anything that failed, then final commit**

Each fix follows TDD where the bug is reachable by a unit test (add the test to the owning module first). Then:

```bash
cargo test -- --test-threads=1
git add -A
git commit -m "fix: address end-to-end findings"   # only if there were changes
```
