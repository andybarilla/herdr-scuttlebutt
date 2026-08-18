use crate::herd::{AgentInfo, HerdControl};
use crate::log_store;
use crate::state::DaemonState;
use anyhow::Result;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const MAX_BATCH_FAILURES: u32 = 5;

fn pid_alive(pid: u32) -> bool {
    // signal 0: existence check, no signal actually delivered
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

pub(crate) fn log_line(dir: &Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
    {
        let _ = writeln!(f, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}

/// Writes to both stderr and daemon.log. The real launch path
/// (`scripts/daemon-ctl.sh`) sends stderr to /dev/null, so daemon.log is the
/// only place these ever land: anything an operator must see goes here, not
/// through a bare `eprintln!`.
pub(crate) fn report(dir: &Path, line: &str) {
    eprintln!("{line}");
    log_line(dir, line);
}

/// Runs one tick and, only on success, persists the resulting state. A
/// failed tick (e.g. `herdr agent list` erroring partway through) must not
/// be saved: `tick` can advance some agents' cursors before returning Err,
/// and persisting that half-applied state would silently drop messages for
/// agents whose delivery never happened this round. The next successful
/// tick re-derives from the last known-good state instead.
fn tick_and_save(state: &mut DaemonState, herd: &dyn HerdControl, dir: &Path) {
    match tick(state, herd, dir) {
        Ok(()) => {
            if let Err(e) = crate::state::save(dir, state) {
                report(dir, &format!("state save error: {e}"));
            }
        }
        Err(e) => {
            report(dir, &format!("tick error: {e}"));
        }
    }
}

pub fn run(dir: &Path) -> Result<()> {
    if let Some(pid) = read_live_pid(dir) {
        report(dir, &format!("daemon already running (pid {pid})"));
        anyhow::bail!("daemon already running (pid {pid})");
    }
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))?;
    if let Err(e) = std::fs::write(dir.join("daemon.pid"), std::process::id().to_string()) {
        // Without a pidfile, daemon-status/daemon-stop cannot find us; fail
        // loudly rather than running unmanageable.
        report(dir, &format!("failed to write daemon.pid: {e}"));
        return Err(e.into());
    }
    log_line(dir, &format!("daemon started; room dir {}", dir.display()));
    let herd = crate::herd::RealHerd;
    let mut state = crate::state::load(dir);
    while !term.load(Ordering::Relaxed) {
        tick_and_save(&mut state, &herd, dir);
        // Sleep for the 2s tick interval in 100ms slices, checking the term
        // flag between slices, so a signal arriving mid-interval is acted on
        // within ~100ms instead of waiting out the full 2s.
        for _ in 0..20 {
            if term.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
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

/// Consecutive absences from `herdr agent list` tolerated before an agent's
/// state (cursor, intro flag, fail count) is purged.
const MAX_ABSENCES: u32 = 3;

pub fn tick(state: &mut DaemonState, herd: &dyn HerdControl, dir: &Path) -> Result<()> {
    let agents = herd.list_agents()?;
    let tail = log_store::last_id(dir)?;
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "scuttlebutt".to_string());

    let live: std::collections::HashSet<String> =
        agents.iter().map(|a| a.name.clone()).collect();

    // enroll new agents (cursor starts at tail: no history dump) and clear
    // any absence streak for agents that are present again.
    for a in &agents {
        state.cursors.entry(a.name.clone()).or_insert(tail);
        state.absences.remove(&a.name);
    }

    // agents we have any state for but that are missing from this listing:
    // tolerate transient absences, purge only after MAX_ABSENCES in a row.
    let known: std::collections::HashSet<String> = state
        .cursors
        .keys()
        .cloned()
        .chain(state.introduced.iter().cloned())
        .chain(state.fail_counts.keys().cloned())
        .chain(state.absences.keys().cloned())
        .collect();
    for name in known {
        if live.contains(&name) {
            continue;
        }
        let count = state.absences.entry(name.clone()).or_insert(0);
        *count += 1;
        if *count >= MAX_ABSENCES {
            state.cursors.remove(&name);
            state.introduced.remove(&name);
            state.fail_counts.remove(&name);
            state.absences.remove(&name);
        }
    }

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
                    report(dir, &format!("[scuttlebutt] intro to {} failed: {e}", a.name));
                }
            }
            // Status was read at the top of this tick; deliver the batch on
            // a later tick against a freshly-read status instead of
            // double-prompting the agent while it's still busy with intro.
            continue;
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
                let entry = state
                    .fail_counts
                    .entry(a.name.clone())
                    .or_insert((0, max_id));
                if entry.1 != max_id {
                    // A new message landed since the last failure: this is a
                    // different batch, so the failure streak restarts.
                    *entry = (0, max_id);
                }
                entry.0 += 1;
                let fails = entry.0;
                report(
                    dir,
                    &format!(
                        "[scuttlebutt] delivery to {} failed ({}/{MAX_BATCH_FAILURES}): {e}",
                        a.name, fails
                    ),
                );
                if fails >= MAX_BATCH_FAILURES {
                    report(
                        dir,
                        &format!(
                            "[scuttlebutt] SKIPPING batch up to #{max_id} for {} after \
                             {MAX_BATCH_FAILURES} failures",
                            a.name
                        ),
                    );
                    state.cursors.insert(a.name.clone(), max_id);
                    state.fail_counts.remove(&a.name);
                }
            }
        }
    }
    Ok(())
}

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
        fail_agents: bool,
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
                fail_agents: false,
            }
        }
    }

    impl HerdControl for FakeHerd {
        fn list_agents(&self) -> anyhow::Result<Vec<AgentInfo>> {
            if self.fail_agents {
                anyhow::bail!("herdr agent list failed");
            }
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
    fn mixed_batch_filters_own_messages_only() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![("reviewer", "idle")]);
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer"]);
        state.cursors.insert("reviewer".into(), 0);
        append(dir.path(), "reviewer", "my own post").unwrap();
        append(dir.path(), "human", "from someone else").unwrap();
        tick(&mut state, &herd, dir.path()).unwrap();
        let prompts = herd.prompts.borrow();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].1.contains("from someone else"));
        assert!(!prompts[0].1.contains("my own post"));
        assert_eq!(state.cursors["reviewer"], 2);
    }

    #[test]
    fn deliverable_includes_idle_and_done_excludes_others() {
        assert!(deliverable("idle"));
        assert!(deliverable("done"));
        assert!(!deliverable("working"));
        assert!(!deliverable("blocked"));
        assert!(!deliverable("unknown"));
    }

    #[test]
    fn vanished_agent_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![]);
        let mut state = DaemonState::default();
        state.cursors.insert("ghost".into(), 3);
        state.introduced.insert("ghost".into());
        // MAX_ABSENCES consecutive misses purge the agent's state.
        for _ in 0..MAX_ABSENCES {
            tick(&mut state, &herd, dir.path()).unwrap();
        }
        assert!(state.cursors.is_empty());
        assert!(state.introduced.is_empty());
    }

    #[test]
    fn transient_absence_keeps_state_and_skips_reintroduction() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer"]);
        state.cursors.insert("reviewer".into(), 5);

        // reviewer is missing from the listing for a single tick
        let herd_absent = FakeHerd::new(vec![]);
        tick(&mut state, &herd_absent, dir.path()).unwrap();
        assert_eq!(state.cursors.get("reviewer"), Some(&5));
        assert!(state.introduced.contains("reviewer"));

        // reviewer reappears before hitting the absence cap: no re-intro
        let herd_back = FakeHerd::new(vec![("reviewer", "idle")]);
        tick(&mut state, &herd_back, dir.path()).unwrap();
        assert!(herd_back.prompts.borrow().is_empty());
        assert_eq!(state.cursors.get("reviewer"), Some(&5));
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
        for _ in 0..(MAX_BATCH_FAILURES - 1) {
            tick(&mut state, &herd, dir.path()).unwrap();
        }
        // not yet at the cap: the batch is still pending, cursor unmoved
        assert_eq!(state.cursors["reviewer"], 0);
        assert_eq!(state.fail_counts["reviewer"].0, MAX_BATCH_FAILURES - 1);

        tick(&mut state, &herd, dir.path()).unwrap();
        // after the 5th consecutive failure the batch is skipped
        assert_eq!(state.cursors["reviewer"], 1);
        assert_eq!(state.fail_counts.get("reviewer"), None);
    }

    #[test]
    fn new_message_resets_fail_streak_for_batch() {
        let dir = tempfile::tempdir().unwrap();
        let mut herd = FakeHerd::new(vec![("reviewer", "idle")]);
        herd.fail_prompts = true;
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer"]);
        state.cursors.insert("reviewer".into(), 0);
        append(dir.path(), "human", "one").unwrap();
        for _ in 0..(MAX_BATCH_FAILURES - 1) {
            tick(&mut state, &herd, dir.path()).unwrap();
        }
        assert_eq!(state.fail_counts["reviewer"].0, MAX_BATCH_FAILURES - 1);

        // a new message grows the batch: this is not the same batch anymore
        append(dir.path(), "human", "two").unwrap();
        tick(&mut state, &herd, dir.path()).unwrap();
        // streak restarted at 1, well under the cap, so nothing was skipped
        assert_eq!(state.cursors["reviewer"], 0);
        assert_eq!(state.fail_counts["reviewer"].0, 1);
    }

    #[test]
    fn success_resets_fail_count() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![("reviewer", "idle")]);
        let mut state = DaemonState::default();
        introduced(&mut state, &["reviewer"]);
        state.cursors.insert("reviewer".into(), 0);
        state.fail_counts.insert("reviewer".into(), (3, 1));
        append(dir.path(), "human", "hello").unwrap();
        tick(&mut state, &herd, dir.path()).unwrap();
        assert_eq!(state.cursors["reviewer"], 1);
        assert_eq!(state.fail_counts.get("reviewer"), None);
    }

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
        // pid 4194304 sits at the 64-bit linux pid_max cap; nothing is
        // realistically alive there
        std::fs::write(dir.path().join("daemon.pid"), "4194304").unwrap();
        assert_eq!(read_live_pid(dir.path()), None);
    }

    #[test]
    fn tick_and_save_skips_save_when_tick_errors() {
        let dir = tempfile::tempdir().unwrap();
        let mut herd = FakeHerd::new(vec![("reviewer", "idle")]);
        herd.fail_agents = true;
        let mut state = DaemonState::default();
        state.cursors.insert("reviewer".into(), 5);
        tick_and_save(&mut state, &herd, dir.path());
        // tick errored before touching state; nothing should be persisted,
        // so a half-applied tick can never be written to state.json.
        assert!(!dir.path().join("state.json").exists());
    }

    #[test]
    fn tick_and_save_persists_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let herd = FakeHerd::new(vec![]);
        let mut state = DaemonState::default();
        tick_and_save(&mut state, &herd, dir.path());
        assert!(dir.path().join("state.json").exists());
    }
}
