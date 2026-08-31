use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
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
    /// Agents whose batch or unconfirmed streak reached
    /// `MAX_FAILURES_BEFORE_STALL` with nothing ever confirming a delivery.
    /// While an agent is in here the daemon leaves its cursor alone and
    /// drops to a widening backoff instead of prompting
    /// it every tick, so the batch survives until its pane can take it — the
    /// alternative, advancing the cursor, loses those messages outright
    /// (#39).
    ///
    /// The backoff rather than silence is what makes the recover-in-place
    /// exit reachable: such a pane reports the same session id throughout,
    /// so a confirmed delivery is the only evidence it is well again, and
    /// nothing is confirmed if nothing is ever sent. That retry is gated on
    /// identity too (#58), so it is an exit only for a pane herdr still
    /// reports the stall's own id at. An agent herdr reports no session id
    /// for has no automatic exit at all: its batch waits for a human.
    ///
    /// Keyed by agent and never by batch: an entry that re-armed when the
    /// batch grew would restart the run-up to the cap on every new room
    /// message, which is the redelivery loop the threshold exists to stop.
    ///
    /// Every agent in here is one herdr is still listing. An entry whose
    /// agent goes away is moved to `held` rather than purged with the
    /// presence state (#43), and comes back here if that name is resumed.
    #[serde(default)]
    pub stalled: HashMap<String, Stall>,
    /// The most recent `agent_session` id herdr reported for each agent.
    /// The field is optional per *listing*, not per agent, so a tick that
    /// omits it says nothing about the process at that pane — and an agent
    /// seen before is one we already know the id of. Keeping it is what
    /// lets a stall record a real id even when it opens on a listing that
    /// dropped the field; without that, the stall records `None`, the next
    /// listing looks like a new session, and delivery goes back to full
    /// rate.
    ///
    /// Scoped to an unbroken presence: the entry is dropped on the agent's
    /// first absence, not with the rest of its state at `MAX_ABSENCES`. A
    /// dropped *field* while the agent stays listed is the same process,
    /// which is the case this map exists for; a broken *presence* is not,
    /// because a different agent can take the name before the purge and
    /// would otherwise be handed the id its predecessor left behind (#43).
    ///
    /// So absent here means one of two things — herdr has never reported an
    /// id for that agent, which is the case for the agent kinds that do not
    /// have them, or the name's presence has broken since it last did.
    /// Both mean the same thing to a reader: nothing here knows who is at
    /// that pane.
    #[serde(default)]
    pub last_session: HashMap<String, String>,
    /// Batches held for agents that are no longer present. The absence
    /// purge clears presence state on its own schedule (six seconds at the
    /// 2s tick), which is shorter than closing and reopening a pane — the
    /// most natural way a human clears a wedge. Taking the stall with the
    /// presence state lost the batch on exactly that path (#43), so the
    /// stall moves here instead, carrying the cursor delivery must resume
    /// from. Nothing here expires on a timer: an entry leaves when its
    /// batch is delivered, when a human drops it, or when the room's cap
    /// evicts the oldest to bound the file.
    #[serde(default)]
    pub held: HashMap<String, Held>,
    /// Held batches this room's cap evicted, newest last and capped at
    /// `MAX_DROPPED_NOTES`. A note is not the batch: those messages are
    /// still in the room log and nothing will deliver them. It stays until
    /// a human acknowledges it with `held <agent> --drop` or another
    /// eviction pushes it out.
    #[serde(default)]
    pub dropped: Vec<Dropped>,
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
    /// Highest message id at the moment the stall opened. Never moves, so
    /// the reports that say when the hold began stay true however long it
    /// lasts; `batch` is the other half of that pair and moves.
    pub held_since: u64,
    /// Highest message id in the batch being held, refreshed on every retry.
    /// Reported and shown by `daemon-status` so a human can see what is
    /// waiting. Normal delivery does not gate on it, and a batch that grows
    /// past it neither clears the stall nor shortens the wait. A human
    /// `held --drop` advances the cursor through this id.
    pub batch: u64,
    /// The last id herdr reported for this agent as of the stall opening or
    /// its last failed retry — not necessarily the one carried by that
    /// particular listing, which may have omitted the field. `None` only
    /// when herdr has never reported one.
    ///
    /// This gates both ways out of a stall that need no human, and it is
    /// asked a different question on each (#58). The lift asks whether the
    /// pane is demonstrably a *different* process: two ids that are `Some`
    /// and unequal, and only while `presence_broken` is false. The retry
    /// asks the opposite — whether it is demonstrably the *same* one, which
    /// is two ids that are `Some` and equal. A `None` on either side answers
    /// neither question, so it refuses both, and the batch waits for a human
    /// rather than going to whoever answers to the name next.
    pub session: Option<String>,
    /// Whether this name has gone missing from `herdr agent list` since the
    /// stall was recorded. Set by the absence loop and never cleared while
    /// the stall stands: one absence is enough to make a later id at that
    /// pane equally consistent with a different agent having taken the name,
    /// which is what the lift must not read as a restart (#58). It does not
    /// gate the retry, which asks for sameness and gets it only from equal
    /// ids.
    ///
    /// Defaults to false for a stall written before this field existed. Such
    /// a stall records nothing about its presence either way, and the
    /// permissive reading is only reachable for one that was recorded, went
    /// absent, and was still inside the four-second window when the daemon
    /// was replaced under it.
    #[serde(default)]
    pub presence_broken: bool,
    /// A human's authorization to deliver this stalled batch to the pane at
    /// this name now. It carries the same identity and 30-minute wall-clock
    /// bound as a held batch's release; a bare flag would arm delivery to a
    /// different process that later took the name.
    ///
    /// Cleared the moment the name goes absent: the authorization was for
    /// the pane that was there, and a name that has left the listing no
    /// longer names it.
    #[serde(default)]
    pub release: Option<Release>,
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
            held_since: batch,
            batch,
            session,
            presence_broken: false,
            release: None,
            waited: 0,
            retries: 0,
        }
    }
}

/// A batch whose agent has gone from `herdr agent list` entirely. Split
/// from `Stall`, which is about an agent that is still there and not
/// taking deliveries: the two are reported separately because a human can
/// fix one at the pane and the other has no pane to fix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Held {
    /// Where redelivery resumes. The cursor as of the purge, so it still
    /// sits below every message the stall was holding.
    pub cursor: u64,
    /// Highest message id when the stall opened, carried over so the
    /// reports that say when the hold began stay true across the purge.
    pub held_since: u64,
    /// Highest message id known to be waiting, for `daemon-status`. Nothing
    /// gates on it.
    pub batch: u64,
    /// The session id herdr last reported for the agent that owned this
    /// batch. Equality with a returning agent's id is the only evidence
    /// that the name still means the same process, which is what gates
    /// automatic redelivery (#43). `None` means no such evidence exists —
    /// either herdr never reported an id for that agent, or two stalls
    /// merged under this name and disagreed about whose batch it is, which
    /// leaves the hold unable to say. Both refuse every returning agent
    /// until a human releases it.
    pub session: Option<String>,
    /// Whether the daemon has already said this batch is held for a name
    /// it cannot match to the agent now using it. Once per record, so a
    /// standing mismatch does not print a line every tick; `daemon-status`
    /// is where it stays visible.
    #[serde(default)]
    pub warned: bool,
    /// A human's standing answer to the question the session id could not
    /// (`scuttlebutt held <agent> --deliver`), or `None` for a hold nobody
    /// has released. Bounded and identified rather than a bare flag: a
    /// release that never expired and compared nothing would arm a
    /// delivery for whoever next answered to that name, which is the
    /// cross-delivery the automatic path exists to refuse.
    #[serde(default)]
    pub release: Option<Release>,
}

/// One human authorization to deliver a held batch. An authorization, not a
/// standing arrangement: it names the process it was given for whenever
/// herdr could report one, and it lapses either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// The session id the agent at that name was reporting when the human
    /// released the hold. `Some` means the release is for that process and
    /// no other. `None` means herdr reported none — the agent was gone, or
    /// is a kind that has no id — and the window below is then the only
    /// thing standing between the release and an unrelated pane.
    pub session: Option<String>,
    /// When the release was given, RFC3339. Wall-clock rather than ticks
    /// because a room whose agents have all gone does not tick at all, and
    /// a release that only lapsed while something was running would stand
    /// forever in exactly the room where it is most likely to be stale.
    ///
    /// This is a bound on the *authorization*, never on the batch: a hold
    /// whose release lapses is still held, and still listed. A stamp in
    /// the future is read as lapsed rather than as fresh, so a state file
    /// written by a machine whose clock runs ahead cannot hold one open.
    pub at: String,
}

/// A held batch the room's cap evicted. Kept so `daemon-status` can still
/// name what was dropped: a batch that leaves the state file with only a
/// `daemon.log` line behind is invisible to the interface that is supposed
/// to be the record of what is waiting. Capped like the holds are, and
/// trimmed on every tick rather than only when one is written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dropped {
    pub agent: String,
    pub batch: u64,
    pub held_since: u64,
    pub at: String,
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

/// Serializes state writers for one room. The lock has its own stable inode:
/// `save` atomically replaces `state.json`, so locking that file would leave a
/// waiter holding the replaced inode instead of the current state file.
pub fn lock_room(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("state.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
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
    fn a_held_batch_roundtrips() {
        // The cursor is the whole point of the record: a hold that came
        // back without it would resume at tail, which is the loss.
        let dir = tempfile::tempdir().unwrap();
        let mut s = DaemonState::default();
        s.held.insert(
            "reviewer".into(),
            Held {
                cursor: 12,
                held_since: 13,
                batch: 19,
                session: Some("sess-1".into()),
                warned: true,
                release: None,
            },
        );
        save(dir.path(), &s).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.held["reviewer"].cursor, 12);
        assert_eq!(loaded.held["reviewer"].session.as_deref(), Some("sess-1"));
        assert!(loaded.held["reviewer"].warned);
        assert!(loaded.held["reviewer"].release.is_none());
    }

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
        // same run of failures to reach the same stall.
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
        assert!(loaded.held.is_empty());
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
