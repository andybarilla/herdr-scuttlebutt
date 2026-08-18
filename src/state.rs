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
