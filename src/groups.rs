use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The three config states the spec distinguishes. `Broken` must never be
/// treated as `Inactive`: silently degrading a malformed config into the
/// single shared room would merge two companies' agents, which is the exact
/// outcome this feature exists to prevent.
#[derive(Debug)]
pub enum Grouping {
    Inactive,
    Active(GroupRules),
    Broken(String),
}

#[derive(Debug, Default)]
pub struct GroupRules {
    /// (group name, absolute prefix), one entry per prefix.
    rules: Vec<(String, PathBuf)>,
}

impl GroupRules {
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.rules.iter().map(|(n, _)| n.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn prefixes_for(&self, group: &str) -> Vec<&Path> {
        self.rules
            .iter()
            .filter(|(n, _)| n == group)
            .map(|(_, p)| p.as_path())
            .collect()
    }
}

#[derive(Deserialize)]
struct RawConfig {
    groups: BTreeMap<String, Vec<String>>,
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

fn valid_group_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn load(base: &Path) -> Grouping {
    let path = base.join("groups.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Grouping::Inactive,
        Err(e) => {
            return Grouping::Broken(format!("{} is unreadable: {e}", path.display()));
        }
    };
    let raw: RawConfig = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => return Grouping::Broken(format!("{} is not valid TOML: {e}", path.display())),
    };
    if raw.groups.is_empty() {
        return Grouping::Broken(format!(
            "{} defines no groups; remove the file to disable grouping",
            path.display()
        ));
    }
    let mut rules = Vec::new();
    for (name, prefixes) in raw.groups {
        if !valid_group_name(&name) {
            return Grouping::Broken(format!(
                "{}: invalid group name {name:?} (allowed: [a-z0-9][a-z0-9_-]*)",
                path.display()
            ));
        }
        if prefixes.is_empty() {
            return Grouping::Broken(format!("{}: group {name:?} has no paths", path.display()));
        }
        for p in prefixes {
            let normalized = normalize(&expand_tilde(&p));
            if normalized.as_os_str().is_empty() || normalized == Path::new("/") {
                return Grouping::Broken(format!(
                    "{}: group {name:?} has an empty or root prefix {p:?}; \
                     a rule that matches every path would merge every agent into one group",
                    path.display()
                ));
            }
            rules.push((name.clone(), normalized));
        }
    }
    Grouping::Active(GroupRules { rules })
}

/// Strips trailing separators so `/a/b/` and `/a/b` compare equal.
fn normalize(p: &Path) -> PathBuf {
    PathBuf::from(p.to_string_lossy().trim_end_matches('/').to_string())
}

/// Longest matching prefix wins — TOML table order is not dependable, so
/// first-match-wins would be nondeterministic, and longest-prefix also lets a
/// nested rule override its parent. Matching is on path-segment boundaries via
/// `Path::starts_with`, so `/dev/alare` never matches `/dev/alarehouse`.
pub fn group_for<'a>(cwd: &Path, rules: &'a GroupRules) -> Option<&'a str> {
    let cwd = normalize(cwd);
    rules
        .rules
        .iter()
        .filter(|(_, prefix)| cwd.starts_with(prefix))
        .max_by_key(|(_, prefix)| prefix.components().count())
        .map(|(name, _)| name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &[&str])]) -> GroupRules {
        let mut v = Vec::new();
        for (name, paths) in pairs {
            for p in *paths {
                v.push((name.to_string(), PathBuf::from(*p)));
            }
        }
        GroupRules { rules: v }
    }

    #[test]
    fn matches_exact_prefix_and_descendants() {
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(group_for(Path::new("/home/a/dev/alare"), &r), Some("alare"));
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api/src"), &r),
            Some("alare")
        );
    }

    #[test]
    fn respects_path_segment_boundaries() {
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(group_for(Path::new("/home/a/dev/alarehouse"), &r), None);
    }

    #[test]
    fn longest_prefix_wins_regardless_of_order() {
        let r = rules(&[
            ("outer", &["/home/a/dev"]),
            ("inner", &["/home/a/dev/alare"]),
        ]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api"), &r),
            Some("inner")
        );
        assert_eq!(group_for(Path::new("/home/a/dev/other"), &r), Some("outer"));

        // reversed declaration order must give the same answer
        let r2 = rules(&[
            ("inner", &["/home/a/dev/alare"]),
            ("outer", &["/home/a/dev"]),
        ]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api"), &r2),
            Some("inner")
        );
    }

    #[test]
    fn multiple_prefixes_map_to_one_group() {
        let r = rules(&[(
            "alare",
            &["/home/a/dev/alare", "/home/a/.herdr/worktrees/alare"],
        )]);
        assert_eq!(
            group_for(Path::new("/home/a/.herdr/worktrees/alare/issue-1"), &r),
            Some("alare")
        );
    }

    #[test]
    fn trailing_slashes_are_ignored() {
        let r = rules(&[("alare", &["/home/a/dev/alare/"])]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/api/"), &r),
            Some("alare")
        );
    }

    #[test]
    fn nonexistent_path_still_resolves() {
        // the daemon must classify an agent whose worktree was deleted
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(
            group_for(Path::new("/home/a/dev/alare/deleted-worktree"), &r),
            Some("alare")
        );
    }

    #[test]
    fn no_match_is_none() {
        let r = rules(&[("alare", &["/home/a/dev/alare"])]);
        assert_eq!(group_for(Path::new("/tmp/scratch"), &r), None);
    }

    #[test]
    fn missing_file_is_inactive() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(load(dir.path()), Grouping::Inactive));
    }

    #[test]
    fn valid_file_is_active() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/home/a/dev/alare\"]\n",
        )
        .unwrap();
        match load(dir.path()) {
            Grouping::Active(r) => {
                assert_eq!(r.names(), vec!["alare"]);
                assert_eq!(
                    group_for(Path::new("/home/a/dev/alare/x"), &r),
                    Some("alare")
                );
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[test]
    fn malformed_file_fails_closed_not_inactive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("groups.toml"), "this is not toml {{{").unwrap();
        match load(dir.path()) {
            Grouping::Broken(msg) => assert!(!msg.is_empty()),
            other => panic!("malformed config must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn invalid_group_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\n\"../escape\" = [\"/home/a/dev/x\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }

    #[test]
    fn empty_groups_table_is_broken_not_silently_empty() {
        // an empty table would enroll nobody while looking healthy
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("groups.toml"), "[groups]\n").unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/dev/x"), PathBuf::from(format!("{home}/dev/x")));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn root_prefix_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\ncatchall = [\"/\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }
}
