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
}

pub fn load(dir: &Path) -> DaemonState {
    let path = dir.join("state.json");
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // Missing file is the normal first-run case: stay silent.
        Err(_) => return DaemonState::default(),
    };
    match serde_json::from_str(&contents) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[scuttlebutt] failed to parse {}: {e}; delivery cursors reset, \
                 all agents will be re-introduced",
                path.display()
            );
            DaemonState::default()
        }
    }
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
    fn missing_file_yields_default_silently() {
        let dir = tempfile::tempdir().unwrap();
        // No state.json at all is the normal first-run case.
        assert!(!dir.path().join("state.json").exists());
        assert!(load(dir.path()).cursors.is_empty());
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
