use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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

/// Where one room's `room.jsonl` and `state.json` live under `session`.
/// `None` is the ungrouped layout (grouping inactive) and keeps v1's paths
/// exactly.
///
/// Pure, and deliberately so: `room_dir` below creates the directory, which
/// a caller that only wants to *look* at a room must not do. Sweeping every
/// room's log length on each picker open would otherwise conjure a
/// directory per room per open — the empty-directory-forever problem
/// `groups::has_history` exists to filter out.
pub fn room_dir_in(session: &Path, group: Option<&str>) -> PathBuf {
    match group {
        Some(g) => session.join(g),
        None => session.to_path_buf(),
    }
}

/// The directory holding one room's `room.jsonl` and `state.json`, created
/// if it does not exist. For a room about to be written to or read as the
/// current room; use `room_dir_in` to merely name one.
pub fn room_dir(group: Option<&str>) -> Result<PathBuf> {
    let dir = room_dir_in(&session_dir()?, group);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The `scuttlebutt` path to advertise to another process.
///
/// Prefers the plugin root, which survives a reinstall; the daemon's own
/// `current_exe()` is the checkout it started from, and a reinstall moves
/// that aside and deletes it, leaving a path that fails. The check for a
/// file under the plugin root is load-bearing: preferring that root
/// unconditionally would trade a path that died for one that may never have
/// existed, and
/// `HERDR_PLUGIN_ROOT` is absent from processes not launched by a herdr
/// plugin action.
pub fn command_path() -> String {
    if let Ok(root) = std::env::var("HERDR_PLUGIN_ROOT") {
        // An empty value would make the join relative, and a daemon whose cwd
        // happens to be a checkout would then advertise a path each agent
        // resolves against its own cwd.
        if !root.is_empty() {
            let bin = PathBuf::from(root).join("target/release/scuttlebutt");
            if bin.is_file() {
                return bin.display().to_string();
            }
        }
    }
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "scuttlebutt".to_string())
}

/// Tests that mutate process-global env vars every path helper reads cannot
/// run concurrently: without this they interleave and one asserts against
/// another's SCUTTLEBUTT_DIR. Any test asserting on `command_path`'s output —
/// including through the daemon's intro text — needs this guard too.
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_sanitizes_socket_path() {
        let _env = env_guard();
        std::env::set_var("HERDR_SOCKET_PATH", "/home/andy/.config/herdr/herdr.sock");
        assert_eq!(session_key(), "-home-andy--config-herdr-herdr-sock");
    }

    #[test]
    fn base_dir_prefers_env_override() {
        let _env = env_guard();
        std::env::set_var("SCUTTLEBUTT_DIR", "/tmp/sb-test");
        assert_eq!(base_dir().unwrap(), PathBuf::from("/tmp/sb-test"));
    }

    #[test]
    fn room_dir_appends_group_when_given() {
        let _env = env_guard();
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
        let _env = env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SCUTTLEBUTT_DIR", dir.path());
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/s.sock");
        assert_eq!(session_dir().unwrap(), room_dir(None).unwrap());
        std::env::remove_var("SCUTTLEBUTT_DIR");
        std::env::remove_var("HERDR_SOCKET_PATH");
    }

    #[test]
    fn command_path_prefers_an_existing_plugin_root_binary() {
        let _env = env_guard();
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("target/release/scuttlebutt");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"").unwrap();
        std::env::set_var("HERDR_PLUGIN_ROOT", root.path());
        assert_eq!(command_path(), bin.display().to_string());
        std::env::remove_var("HERDR_PLUGIN_ROOT");
    }

    #[test]
    fn command_path_falls_back_when_the_plugin_root_holds_no_binary() {
        let _env = env_guard();
        let root = tempfile::tempdir().unwrap();
        std::env::set_var("HERDR_PLUGIN_ROOT", root.path());
        assert_eq!(command_path(), own_exe());
        std::env::remove_var("HERDR_PLUGIN_ROOT");
    }

    #[test]
    fn command_path_falls_back_when_the_plugin_root_is_unset() {
        let _env = env_guard();
        std::env::remove_var("HERDR_PLUGIN_ROOT");
        assert_eq!(command_path(), own_exe());
    }

    #[test]
    fn command_path_ignores_an_empty_plugin_root() {
        let _env = env_guard();
        std::env::set_var("HERDR_PLUGIN_ROOT", "");
        assert_eq!(command_path(), own_exe());
        std::env::remove_var("HERDR_PLUGIN_ROOT");
    }

    fn own_exe() -> String {
        std::env::current_exe().unwrap().display().to_string()
    }
}
