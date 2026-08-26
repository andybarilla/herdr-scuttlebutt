use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DaemonState {
    pub cursors: HashMap<String, u64>,
    pub introduced: HashSet<String>,
    /// (consecutive failure count, batch max message id) per agent. Restarts
    /// whenever the batch grows, so on its own it converges only in a quiet
    /// room; `unconfirmed_streak` is what bounds the busy one.
    #[serde(default)]
    pub fail_counts: HashMap<String, (u32, u64)>,
    /// Consecutive ticks an enrolled agent has been absent from `herdr agent list`.
    #[serde(default)]
    pub absences: HashMap<String, u32>,
    /// Consecutive ticks a not-yet-introduced agent has been deliverable and
    /// unfocused. Broken by either, so the intro waits for fresh sightings.
    #[serde(default)]
    pub deliverable_streak: HashMap<String, u32>,
    /// Consecutive deliveries to an agent that herdr accepted without
    /// confirming submitted. Separate from `fail_counts`, which restarts
    /// whenever the batch grows: in a room with traffic that streak never
    /// reaches the threshold, so an agent whose pane never submits would be
    /// re-prompted every tick forever. This one is batch-independent and
    /// cleared only by a confirmed delivery or by the stall it leads to
    /// being lifted, which is what makes it converge. Outright prompt errors do not touch it — those keep
    /// `fail_counts`' per-batch semantics, where a bigger batch is worth
    /// another try.
    #[serde(default)]
    pub unconfirmed_streak: HashMap<String, u32>,
    /// Consecutive intro prompt failures per agent.
    #[serde(default)]
    pub intro_fails: HashMap<String, u32>,
    /// Agents whose batch reached `MAX_BATCH_FAILURES` with nothing ever
    /// confirming a delivery. While an agent is in here the daemon leaves
    /// its cursor alone and drops to a widening backoff instead of prompting
    /// it every tick, so the batch survives until its pane can take it — the
    /// alternative, advancing the cursor, loses those messages outright
    /// (#39).
    ///
    /// The backoff rather than silence is what makes the exits reachable: a
    /// pane that recovers in place reports the same session id and would
    /// never be seen to recover if nothing were ever sent to it again, and
    /// an agent herdr reports no session id for has no other way out at all.
    ///
    /// Keyed by agent and never by batch: an entry that re-armed when the
    /// batch grew would restart the five-failure cycle on every new room
    /// message, which is the redelivery loop the threshold exists to stop.
    #[serde(default)]
    pub stalled: HashMap<String, Stall>,
    /// Agents currently being reported without a `focused` field. The check
    /// runs every tick; the warning is once per outage, and the entry is
    /// dropped as soon as the field comes back so a later outage warns again.
    #[serde(default)]
    pub focus_unknown_warned: HashSet<String>,
}

/// One agent's held batch. Recorded when delivery is given up on, dropped
/// when the pane proves it can receive again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stall {
    /// Highest message id in the batch being held, refreshed on every retry.
    /// Reported and shown by `daemon-status` so a human can see what is
    /// waiting; nothing gates on it, and a batch that grows past it neither
    /// clears the stall nor shortens the wait.
    pub batch: u64,
    /// The agent's session id when it stalled, or when it last failed a
    /// retry; `None` if herdr reported none. A different id means a
    /// different process is at that pane, which lifts the stall at once
    /// rather than waiting out the backoff.
    pub session: Option<String>,
    /// Delivery opportunities counted since the last retry — ticks where
    /// this agent was deliverable and unfocused, not wall-clock seconds. A
    /// pane nobody can be prompted at does not burn its backoff.
    pub waited: u32,
    /// Retries made since the stall opened. Sets how long the next wait is,
    /// and is what widens it.
    pub retries: u32,
}

impl Stall {
    /// A stall as it opens: nothing waited, nothing retried yet.
    pub fn new(batch: u64, session: Option<String>) -> Self {
        Stall {
            batch,
            session,
            waited: 0,
            retries: 0,
        }
    }
}

pub fn load(dir: &Path) -> DaemonState {
    let path = dir.join("state.json");
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // Missing file is the normal first-run case: stay silent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DaemonState::default(),
        // Present but unreadable (permissions, invalid UTF-8) is a state
        // reset just like a parse failure, and must be as loud.
        Err(e) => {
            crate::daemon::report(dir, &reset_warning(&path, &e.to_string()));
            return DaemonState::default();
        }
    };
    match serde_json::from_str(&contents) {
        Ok(s) => s,
        Err(e) => {
            crate::daemon::report(dir, &reset_warning(&path, &e.to_string()));
            DaemonState::default()
        }
    }
}

fn reset_warning(path: &Path, cause: &str) -> String {
    format!(
        "[scuttlebutt] failed to read {}: {cause}; delivery cursors reset, \
         all agents will be re-introduced",
        path.display()
    )
}

pub fn save(dir: &Path, s: &DaemonState) -> Result<()> {
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(tmp, dir.join("state.json"))?;
    Ok(())
}

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
    fn a_stall_survives_a_daemon_restart() {
        // A held batch that did not outlive the daemon would be delivered
        // again on the next tick, and the agent would work back through the
        // same five failures to reach the same stall.
        let dir = tempfile::tempdir().unwrap();
        let mut s = DaemonState::default();
        s.cursors.insert("reviewer".into(), 7);
        s.stalled
            .insert("reviewer".into(), Stall::new(12, Some("session-a".into())));
        save(dir.path(), &s).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.stalled["reviewer"].batch, 12);
        assert_eq!(
            loaded.stalled["reviewer"].session.as_deref(),
            Some("session-a")
        );
    }

    #[test]
    fn a_production_state_file_still_loads() {
        // Verbatim from a live room dir. Cursors are the only record of what
        // each agent has seen, so a shape change that reset them would lose
        // every one of them silently.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("state.json"),
            r#"{
              "cursors": { "lead-alare": 319, "ic-alare-1040": 320 },
              "introduced": ["ic-alare-1040", "lead-alare"],
              "fail_counts": { "ic-alare-1040": [2, 320] },
              "absences": {},
              "deliverable_streak": {},
              "intro_fails": {},
              "focus_unknown_warned": []
            }"#,
        )
        .unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.cursors["lead-alare"], 319);
        assert_eq!(loaded.cursors["ic-alare-1040"], 320);
        assert!(loaded.introduced.contains("lead-alare"));
        assert_eq!(loaded.fail_counts["ic-alare-1040"], (2, 320));
        // fields added after this file was written default rather than
        // failing the parse and resetting every cursor
        assert!(loaded.unconfirmed_streak.is_empty());
        assert!(loaded.stalled.is_empty());
        assert!(
            !daemon_log(dir.path()).contains("delivery cursors reset"),
            "an existing state file was rejected"
        );
    }

    fn daemon_log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default()
    }

    #[test]
    fn missing_file_yields_default_silently() {
        let dir = tempfile::tempdir().unwrap();
        // No state.json at all is the normal first-run case.
        assert!(!dir.path().join("state.json").exists());
        assert!(load(dir.path()).cursors.is_empty());
        assert!(!dir.path().join("daemon.log").exists());
    }

    #[test]
    fn corrupt_file_warning_reaches_daemon_log() {
        // The real launch path discards stderr, so the reset warning only
        // counts if it lands in daemon.log.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), "garbage").unwrap();
        load(dir.path());
        let log = daemon_log(dir.path());
        assert!(log.contains("delivery cursors reset"), "log was: {log}");
        assert!(log.contains("state.json"), "log was: {log}");
    }

    #[test]
    fn unreadable_file_warns_instead_of_looking_like_first_run() {
        // Invalid UTF-8: read_to_string fails before serde ever sees it,
        // which used to collapse into the silent "missing file" branch.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), [0xff, 0xfe, 0xfd]).unwrap();
        assert!(load(dir.path()).cursors.is_empty());
        assert!(daemon_log(dir.path()).contains("delivery cursors reset"));
    }

    #[test]
    fn corrupt_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), "garbage").unwrap();
        assert!(load(dir.path()).cursors.is_empty());
        // A shape change (e.g. fail_counts value type) is corrupt from an
        // old state.json's point of view too: still yields Default rather
        // than propagating the parse error.
        std::fs::write(
            dir.path().join("state.json"),
            r#"{"cursors":{},"introduced":[],"fail_counts":{"reviewer":3}}"#,
        )
        .unwrap();
        assert!(load(dir.path()).cursors.is_empty());
    }
}
