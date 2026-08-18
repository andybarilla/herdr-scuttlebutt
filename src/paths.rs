use anyhow::{Context, Result};
use std::path::PathBuf;

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
}
