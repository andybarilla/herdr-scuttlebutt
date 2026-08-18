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
