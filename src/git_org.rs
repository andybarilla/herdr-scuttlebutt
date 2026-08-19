/// Derives a group name from a repository's `origin` remote, used as the
/// fallback when `groups.toml` has no prefix for a working directory.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a lookup is trusted. Remotes change rarely, but a daemon runs for
/// days across worktrees that are created and deleted under it, so entries
/// expire rather than pinning the first answer forever.
const TTL: Duration = Duration::from_secs(300);

/// The group a working directory belongs to by virtue of its repository, or
/// `None` when the directory is not in a repo, has no `origin`, or is gone.
pub fn org_for(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    org_from_url(std::str::from_utf8(&out.stdout).ok()?)
}

/// Memoizes `org_for` per working directory. The daemon resolves every agent
/// on every 2s tick; without this each tick spawns one `git` per agent.
pub struct OrgCache {
    lookup: fn(&Path) -> Option<String>,
    ttl: Duration,
    entries: HashMap<PathBuf, (Instant, Option<String>)>,
}

impl Default for OrgCache {
    fn default() -> Self {
        Self {
            lookup: org_for,
            ttl: TTL,
            entries: HashMap::new(),
        }
    }
}

impl OrgCache {
    pub fn get(&mut self, cwd: &Path) -> Option<String> {
        if let Some((at, org)) = self.entries.get(cwd) {
            if at.elapsed() < self.ttl {
                return org.clone();
            }
        }
        let org = (self.lookup)(cwd);
        self.entries
            .insert(cwd.to_path_buf(), (Instant::now(), org.clone()));
        org
    }

    #[cfg(test)]
    pub fn with_lookup(lookup: fn(&Path) -> Option<String>, ttl: Duration) -> Self {
        Self {
            lookup,
            ttl,
            entries: HashMap::new(),
        }
    }
}

/// The owner segment of a remote URL, sanitized into a legal group name.
/// Both forge URLs (`scheme://host/owner/repo`) and scp-style remotes
/// (`user@host:owner/repo`) are recognized; a plain local path has no owner
/// and yields `None` rather than a group named after a directory.
pub fn org_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p)?,
        None => {
            // scp syntax only when the colon precedes any slash: `/srv/a:b` is
            // a local path, not `host:path`.
            let colon = url.find(':')?;
            if url[..colon].contains('/') {
                return None;
            }
            &url[colon + 1..]
        }
    };
    let owner = path.split('/').find(|s| !s.is_empty())?;
    sanitize(owner)
}

/// Maps an arbitrary owner string onto `groups::valid_group_name`: lowercase,
/// anything else a `-`, and leading characters dropped until one that may
/// start a name. `None` when nothing usable is left.
fn sanitize(owner: &str) -> Option<String> {
    let mapped: String = owner
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let start = mapped.find(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())?;
    Some(mapped[start..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counting_lookup(_cwd: &Path) -> Option<String> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        Some("acme".to_string())
    }

    fn counting_miss(_cwd: &Path) -> Option<String> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        None
    }

    fn repo_with_origin(url: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", url]);
        dir
    }

    #[test]
    fn reads_the_origin_owner_from_a_real_repo() {
        let dir = repo_with_origin("git@github.com:AcmeCorp/api.git");
        assert_eq!(org_for(dir.path()).as_deref(), Some("acmecorp"));
    }

    #[test]
    fn resolves_from_a_subdirectory_of_the_repo() {
        let dir = repo_with_origin("git@github.com:AcmeCorp/api.git");
        let sub = dir.path().join("crates/inner");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(org_for(&sub).as_deref(), Some("acmecorp"));
    }

    #[test]
    fn a_directory_outside_any_repo_has_no_org() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(org_for(dir.path()), None);
    }

    #[test]
    fn a_vanished_working_directory_has_no_org() {
        // the daemon must classify an agent whose worktree was deleted
        assert_eq!(org_for(Path::new("/nonexistent/worktree/issue-1")), None);
    }

    #[test]
    fn repeat_lookups_of_one_cwd_hit_the_cache() {
        CALLS.store(0, Ordering::SeqCst);
        let mut cache = OrgCache::with_lookup(counting_lookup, Duration::from_secs(300));
        assert_eq!(cache.get(Path::new("/w/a")).as_deref(), Some("acme"));
        assert_eq!(cache.get(Path::new("/w/a")).as_deref(), Some("acme"));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_missing_org_is_cached_too() {
        // otherwise every tick re-spawns git for every non-repo agent
        CALLS.store(0, Ordering::SeqCst);
        let mut cache = OrgCache::with_lookup(counting_miss, Duration::from_secs(300));
        assert_eq!(cache.get(Path::new("/w/b")), None);
        assert_eq!(cache.get(Path::new("/w/b")), None);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_expired_entry_is_looked_up_again() {
        // a cwd becomes a repo, or gains an origin, while the daemon runs
        CALLS.store(0, Ordering::SeqCst);
        let mut cache = OrgCache::with_lookup(counting_lookup, Duration::ZERO);
        cache.get(Path::new("/w/c"));
        cache.get(Path::new("/w/c"));
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn distinct_cwds_are_looked_up_separately() {
        CALLS.store(0, Ordering::SeqCst);
        let mut cache = OrgCache::with_lookup(counting_lookup, Duration::from_secs(300));
        cache.get(Path::new("/w/d"));
        cache.get(Path::new("/w/e"));
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn scp_syntax_yields_the_owner() {
        assert_eq!(
            org_from_url("git@github.com:AcmeCorp/api.git").as_deref(),
            Some("acmecorp")
        );
    }

    #[test]
    fn https_url_yields_the_owner() {
        assert_eq!(
            org_from_url("https://gitlab.com/AcmeCorp/web").as_deref(),
            Some("acmecorp")
        );
    }

    #[test]
    fn ssh_url_with_port_and_subgroup_yields_the_first_segment() {
        assert_eq!(
            org_from_url("ssh://git@host:22/Owner/sub/repo.git").as_deref(),
            Some("owner")
        );
    }

    #[test]
    fn credentials_in_the_url_are_not_mistaken_for_the_owner() {
        assert_eq!(
            org_from_url("https://user:token@github.com/Acme_Corp/x.git").as_deref(),
            Some("acme_corp")
        );
    }

    #[test]
    fn illegal_characters_become_separators() {
        assert_eq!(
            org_from_url("https://host/Acme.Corp/x.git").as_deref(),
            Some("acme-corp")
        );
    }

    #[test]
    fn leading_illegal_characters_are_trimmed_to_keep_the_name_valid() {
        // group names must start [a-z0-9]; `~andy` would otherwise sanitize to
        // `-andy`, which the groups config rejects and which reads as a flag
        assert_eq!(
            org_from_url("git@host:~andy/thing.git").as_deref(),
            Some("andy")
        );
    }

    #[test]
    fn a_local_path_remote_has_no_owner() {
        assert_eq!(org_from_url("/srv/git/bare.git"), None);
        assert_eq!(org_from_url("../sibling"), None);
    }

    #[test]
    fn a_url_without_a_path_has_no_owner() {
        assert_eq!(org_from_url("https://github.com/"), None);
        assert_eq!(org_from_url(""), None);
    }

    #[test]
    fn a_name_with_nothing_usable_is_rejected() {
        assert_eq!(org_from_url("https://host/.../x.git"), None);
    }
}
