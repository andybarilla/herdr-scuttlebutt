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

pub fn room_dir() -> Result<PathBuf> {
    let dir = base_dir()?.join(session_key());
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
}
