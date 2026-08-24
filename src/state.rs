use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DaemonState {
    pub cursors: HashMap<String, u64>,
    pub introduced: HashSet<String>,
    /// (consecutive failure count, batch max message id) per agent.
    #[serde(default)]
    pub fail_counts: HashMap<String, (u32, u64)>,
    /// Consecutive ticks an enrolled agent has been absent from `herdr agent list`.
    #[serde(default)]
    pub absences: HashMap<String, u32>,
    /// Consecutive ticks a not-yet-introduced agent has been deliverable.
    #[serde(default)]
    pub deliverable_streak: HashMap<String, u32>,
    /// Consecutive intro prompt failures per agent.
    #[serde(default)]
    pub intro_fails: HashMap<String, u32>,
    /// Agents already reported as missing the `focused` field. The check runs
    /// every tick; the warning is once per agent.
    #[serde(default)]
    pub focus_unknown_warned: HashSet<String>,
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
