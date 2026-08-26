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

pub fn valid_group_name(name: &str) -> bool {
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
    let mut seen: BTreeMap<PathBuf, String> = BTreeMap::new();
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
            if normalized
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Grouping::Broken(format!(
                    "{}: group {name:?} has a prefix containing `..` ({p:?}); \
                     prefixes are matched literally against absolute working \
                     directories and one with `..` can never match",
                    path.display()
                ));
            }
            if !normalized.is_absolute() {
                return Grouping::Broken(format!(
                    "{}: group {name:?} has a relative prefix {p:?}; \
                     prefixes are matched against absolute working directories \
                     and a relative one can never match",
                    path.display()
                ));
            }
            if let Some(owner) = seen.get(&normalized) {
                if owner != &name {
                    return Grouping::Broken(format!(
                        "{}: prefix {p:?} is claimed by both {owner:?} and {name:?}",
                        path.display()
                    ));
                }
            } else {
                seen.insert(normalized.clone(), name.clone());
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

/// The group a working directory belongs to: a configured prefix if one
/// matches, otherwise the repository's `origin` organization. A `Broken`
/// config derives nothing — falling back to org-derived rooms there would
/// enroll everyone from a config we could not read.
pub fn resolve(
    cwd: &Path,
    grouping: &Grouping,
    orgs: &mut crate::git_org::OrgCache,
) -> Option<String> {
    match grouping {
        Grouping::Broken(_) => None,
        Grouping::Inactive => orgs.get(cwd),
        Grouping::Active(rules) => match group_for(cwd, rules) {
            Some(g) => Some(g.to_string()),
            None => orgs.get(cwd),
        },
    }
}

/// One room a caller could open, carrying the three sources that vouch for
/// it. Three flavours a bare name cannot tell apart: a group with agents in
/// it, a group the config names that nobody is in, and a group that survives
/// only as history on disk. All three are real rooms and a picker has to
/// label them differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    /// `None` is the ungrouped room — `session_dir()` itself rather than a
    /// subdirectory of it, and offered only when grouping is `Inactive`.
    pub name: Option<String>,
    /// Live agents whose cwd resolves here.
    pub agents: usize,
    /// Named by `groups.toml`.
    pub configured: bool,
    /// Holds a `room.jsonl` with something in it.
    pub history: bool,
}

/// One of the three things that can vouch for a room, in provenance order:
/// an agent standing in it now outranks the config naming it, which outranks
/// a log left on disk. An enum rather than a string so a caller grouping
/// rooms under headings matches on the source instead of on its wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Agents,
    Config,
    History,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Agents => "live agents",
            Source::Config => "config",
            Source::History => "history",
        }
    }
}

impl Room {
    /// Every source that vouches for this room, in provenance order, so the
    /// first is the room's primary source. Rooms are genuinely vouched for
    /// by more than one thing at once — `alare` has agents in it, sits in
    /// the config and holds a log — and which ones is the whole reason
    /// `Room` carries three flags rather than a name.
    ///
    /// Labelling and sorting both read from here so they cannot give two
    /// answers about the same room: listing only the config for a room that
    /// also has agents in it labels it as configured while `rooms` sorts it
    /// as live.
    ///
    /// Empty only for a `Room` nothing vouches for, which `rooms` never
    /// builds.
    pub fn sources(&self) -> Vec<Source> {
        [
            (self.agents > 0, Source::Agents),
            (self.configured, Source::Config),
            (self.history, Source::History),
        ]
        .into_iter()
        .filter_map(|(vouches, s)| vouches.then_some(s))
        .collect()
    }

    /// Sort key: where this room's primary source falls in provenance order.
    /// A room nothing vouches for sorts last rather than first.
    pub fn provenance_rank(&self) -> u8 {
        self.sources().first().map_or(u8::MAX, |s| *s as u8)
    }
}

/// The room a chat pane is currently viewing.
///
/// Three states rather than `Option<String>`, which conflates the last two:
/// the ungrouped room is a real room with a real directory that can be read
/// and posted to, while `NoneSelected` is a pane whose cwd resolved to no
/// group and which has nowhere to post until a human picks a room. Collapsed
/// into one `None`, a draft map and `title_for` would disagree about that
/// pane the first time anyone ran without a `groups.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum CurrentRoom {
    Named(String),
    /// `session_dir()` itself — v1's single shared room, offered only when
    /// grouping is `Inactive`.
    Ungrouped,
    #[default]
    NoneSelected,
}

impl CurrentRoom {
    /// The `Option<&str>` group that `room_dir`, `title_for` and
    /// `visible_agents` all take, or `None` when no room is selected. The
    /// nesting is the point: the inner `None` is the ungrouped room, so a
    /// caller has to deal with "nothing selected" before it can ask which
    /// group it is looking at.
    pub fn selected(&self) -> Option<Option<&str>> {
        match self {
            CurrentRoom::Named(n) => Some(Some(n.as_str())),
            CurrentRoom::Ungrouped => Some(None),
            CurrentRoom::NoneSelected => None,
        }
    }

    /// How this room is written wherever a room is named to a human: the
    /// picker rows, the substring filter that searches them, and the pane
    /// title all use this, so what you can see is what you can type.
    pub fn label(&self) -> &str {
        match self {
            CurrentRoom::Named(n) => n.as_str(),
            CurrentRoom::Ungrouped => "(ungrouped)",
            CurrentRoom::NoneSelected => "no room selected",
        }
    }
}

impl From<&Room> for CurrentRoom {
    fn from(r: &Room) -> Self {
        match &r.name {
            Some(n) => CurrentRoom::Named(n.clone()),
            None => CurrentRoom::Ungrouped,
        }
    }
}

/// Whether a room directory holds a `room.jsonl` with anything in it.
/// Emptiness is the filter that matters: `paths::room_dir` calls
/// `create_dir_all` before the first post, so one mistyped `--group` leaves a
/// directory that would otherwise be offered as a room forever.
fn has_history(dir: &Path) -> bool {
    std::fs::metadata(dir.join("room.jsonl"))
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// The accumulating entry for one room name, created blank on first touch.
/// Every source goes through this, which is what makes the union a dedup
/// rather than three lists concatenated.
fn entry_for(found: &mut BTreeMap<Option<String>, Room>, name: Option<String>) -> &mut Room {
    found.entry(name.clone()).or_insert(Room {
        name,
        agents: 0,
        configured: false,
        history: false,
    })
}

/// Every room this session could open, deduped across the three sources that
/// know about rooms and genuinely disagree: the config, the live agents, and
/// the session directory. In a real config `jackdaw` is configured with no
/// room file and `andybarilla` has 42 KB of history and appears in no config,
/// so no one source is the list.
///
/// `Broken` yields nothing, mirroring `visible_agents`. The disk sweep below
/// never consults the config, so enumerating under a config we could not
/// parse would list every company's room — the outcome this module exists to
/// prevent.
pub fn rooms(
    grouping: &Grouping,
    agents: &[crate::herd::AgentInfo],
    session_dir: &Path,
    orgs: &mut crate::git_org::OrgCache,
) -> Vec<Room> {
    if matches!(grouping, Grouping::Broken(_)) {
        return Vec::new();
    }
    let mut found: BTreeMap<Option<String>, Room> = BTreeMap::new();

    if let Grouping::Active(rules) = grouping {
        for name in rules.names() {
            entry_for(&mut found, Some(name.to_string())).configured = true;
        }
    }

    for a in agents {
        match resolve(Path::new(&a.cwd), grouping, orgs) {
            Some(g) => entry_for(&mut found, Some(g)).agents += 1,
            // Without a config, an agent in no repository belongs to the
            // single shared room, so it is a member like any other.
            None if matches!(grouping, Grouping::Inactive) => {
                entry_for(&mut found, None).agents += 1
            }
            // Under a config the same agent is enrolled nowhere, and the
            // ungrouped room receives nothing. Counting it would conjure a
            // room out of one stray cwd whose only member can never be
            // reached in it.
            None => {}
        }
    }

    if let Ok(entries) = std::fs::read_dir(session_dir) {
        for e in entries.flatten() {
            let name = match e.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue,
            };
            // A directory whose name is not a legal group can never be given
            // back to `--group`, so listing it offers a room nothing can open.
            // Only this source needs the check: source 1's names came through
            // `load`, which rejects illegal ones, and source 2's come from
            // `load` or from `git_org::sanitize`, which maps an arbitrary
            // repository owner onto exactly this predicate.
            if !valid_group_name(&name) {
                continue;
            }
            if has_history(&e.path()) {
                entry_for(&mut found, Some(name)).history = true;
            }
        }
    }
    // The ungrouped room is `session_dir()` itself, so it has no entry in the
    // sweep above. Under an active config it receives nothing and neither
    // `resolve_group` nor `resolve_room` can land in it, so its history is
    // the record of a room that can no longer be posted to; offering it —
    // to the chat pane's picker above all, which posts wherever it is
    // pointed — would invite posting into a dead room.
    if matches!(grouping, Grouping::Inactive) && has_history(session_dir) {
        entry_for(&mut found, None).history = true;
    }

    let mut out: Vec<Room> = found.into_values().collect();
    // Never by mtime: a list that reorders between readings defeats muscle
    // memory in the picker this feeds.
    out.sort_by(|a, b| {
        a.provenance_rank().cmp(&b.provenance_rank()).then_with(|| {
            a.name
                .as_deref()
                .unwrap_or("")
                .cmp(b.name.as_deref().unwrap_or(""))
        })
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org_acme(_cwd: &Path) -> Option<String> {
        Some("acme".to_string())
    }

    fn no_org(_cwd: &Path) -> Option<String> {
        None
    }

    fn cache(lookup: fn(&Path) -> Option<String>) -> crate::git_org::OrgCache {
        crate::git_org::OrgCache::with_lookup(lookup, std::time::Duration::from_secs(300))
    }

    #[test]
    fn a_configured_prefix_beats_the_repo_org() {
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        assert_eq!(
            resolve(Path::new("/w/alare/api"), &g, &mut cache(org_acme)).as_deref(),
            Some("alare")
        );
    }

    #[test]
    fn an_unmatched_cwd_falls_back_to_the_repo_org() {
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        assert_eq!(
            resolve(Path::new("/w/other"), &g, &mut cache(org_acme)).as_deref(),
            Some("acme")
        );
    }

    #[test]
    fn an_unmatched_cwd_outside_a_repo_stays_ungrouped() {
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        assert_eq!(resolve(Path::new("/w/other"), &g, &mut cache(no_org)), None);
    }

    #[test]
    fn inactive_grouping_resolves_from_the_repo_org() {
        assert_eq!(
            resolve(
                Path::new("/w/other"),
                &Grouping::Inactive,
                &mut cache(org_acme)
            )
            .as_deref(),
            Some("acme")
        );
    }

    #[test]
    fn inactive_grouping_outside_a_repo_is_the_shared_room() {
        assert_eq!(
            resolve(
                Path::new("/w/other"),
                &Grouping::Inactive,
                &mut cache(no_org)
            ),
            None
        );
    }

    #[test]
    fn a_broken_config_never_derives_a_group() {
        // fail closed: a config we cannot read must enroll nobody, and an
        // org-derived room would quietly enroll everyone instead
        let g = Grouping::Broken("bad".into());
        assert_eq!(
            resolve(Path::new("/w/alare"), &g, &mut cache(org_acme)),
            None
        );
    }

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
        assert_eq!(
            expand_tilde("~/dev/x"),
            PathBuf::from(format!("{home}/dev/x"))
        );
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

    #[test]
    fn duplicate_prefix_across_groups_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/home/a/dev/x\"]\nbeta = [\"/home/a/dev/x\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }

    #[test]
    fn duplicate_prefix_within_same_group_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/home/a/dev/x\", \"/home/a/dev/x\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Active(_)));
    }

    #[test]
    fn relative_prefix_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"dev/alare\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }

    #[test]
    fn parent_dir_prefix_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/w/../alare\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Broken(_)));
    }

    #[test]
    fn current_dir_component_in_prefix_is_accepted() {
        // `components()` elides `.`, so /a/./b really is /a/b
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/w/./alare\"]\n",
        )
        .unwrap();
        match load(dir.path()) {
            Grouping::Active(r) => {
                assert_eq!(group_for(Path::new("/w/alare/api"), &r), Some("alare"))
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[test]
    fn tilde_prefix_is_still_active() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"~/dev/alare\"]\n",
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Grouping::Active(_)));
    }

    // --- rooms() -------------------------------------------------------

    fn agent_at(cwd: &str) -> crate::herd::AgentInfo {
        crate::herd::AgentInfo {
            name: format!("agent{cwd}"),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
            cwd: cwd.into(),
            focused: Some(false),
            session: None,
        }
    }

    /// `/w/<org>/...` belongs to `<org>`; anything else is outside a repo.
    fn fake_org(cwd: &Path) -> Option<String> {
        let rest = cwd.to_string_lossy().strip_prefix("/w/")?.to_string();
        Some(rest.split('/').next()?.to_string())
    }

    /// Writes `text` into a room's log. `None` is the ungrouped room, whose
    /// log lives in the session directory itself.
    fn write_room(session: &Path, name: Option<&str>, text: &str) {
        let dir = match name {
            Some(n) => session.join(n),
            None => session.to_path_buf(),
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("room.jsonl"), text).unwrap();
    }

    fn names(rooms: &[Room]) -> Vec<&str> {
        rooms
            .iter()
            .map(|r| r.name.as_deref().unwrap_or("(ungrouped)"))
            .collect()
    }

    fn room<'a>(rooms: &'a [Room], name: &str) -> &'a Room {
        rooms
            .iter()
            .find(|r| r.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no room {name:?} in {:?}", names(rooms)))
    }

    #[test]
    fn a_broken_config_lists_no_room_at_all() {
        // the disk sweep never reads the config, so a config we could not
        // parse must stop enumeration outright rather than fall through to it
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), Some("acme"), "{}\n");
        write_room(session.path(), None, "{}\n");
        let found = rooms(
            &Grouping::Broken("bad".into()),
            &[agent_at("/w/alare/api")],
            session.path(),
            &mut cache(fake_org),
        );
        assert_eq!(found, Vec::new());
    }

    #[test]
    fn the_ungrouped_room_appears_when_grouping_is_inactive() {
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), None, "{}\n");
        let found = rooms(
            &Grouping::Inactive,
            &[agent_at("/elsewhere")],
            session.path(),
            &mut cache(fake_org),
        );
        assert_eq!(names(&found), vec!["(ungrouped)"]);
        assert_eq!(found[0].agents, 1);
        assert!(found[0].history);
        assert!(!found[0].configured);
    }

    #[test]
    fn the_ungrouped_room_is_hidden_under_an_active_config() {
        // it receives nothing under a config, so offering it would invite
        // posting into a dead room
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), None, "{}\n");
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        let found = rooms(&g, &[], session.path(), &mut cache(fake_org));
        assert_eq!(names(&found), vec!["alare"]);
    }

    #[test]
    fn an_agent_belonging_nowhere_does_not_conjure_an_ungrouped_room() {
        // `resolve` returns None for a cwd in no group and no repo; under a
        // config that means enrolled nowhere, not a member of a shared room
        let session = tempfile::tempdir().unwrap();
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        let found = rooms(
            &g,
            &[agent_at("/tmp/scratch")],
            session.path(),
            &mut cache(no_org),
        );
        assert_eq!(names(&found), vec!["alare"]);
        assert_eq!(room(&found, "alare").agents, 0);
    }

    #[test]
    fn a_directory_holding_an_empty_room_file_is_not_a_room() {
        // `room_dir` creates the directory before the first post, so one
        // mistyped --group leaves an empty room behind
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), Some("typoo"), "");
        std::fs::create_dir_all(session.path().join("nolog")).unwrap();
        write_room(session.path(), Some("real"), "{}\n");
        let found = rooms(&Grouping::Inactive, &[], session.path(), &mut cache(no_org));
        assert_eq!(names(&found), vec!["real"]);
    }

    #[test]
    fn a_configured_group_with_no_room_file_still_appears() {
        let session = tempfile::tempdir().unwrap();
        let g = Grouping::Active(rules(&[("jackdaw", &["/w/jackdaw"])]));
        let found = rooms(&g, &[], session.path(), &mut cache(no_org));
        assert_eq!(names(&found), vec!["jackdaw"]);
        assert!(found[0].configured);
        assert!(!found[0].history);
        assert_eq!(found[0].agents, 0);
    }

    #[test]
    fn a_room_with_history_and_no_config_still_appears() {
        // the real config's `andybarilla`: 42 KB of log, named nowhere
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), Some("andybarilla"), "{}\n");
        let g = Grouping::Active(rules(&[("jackdaw", &["/w/jackdaw"])]));
        let found = rooms(&g, &[], session.path(), &mut cache(no_org));
        let quiet = room(&found, "andybarilla");
        assert!(quiet.history);
        assert!(!quiet.configured);
        // and it is distinguishable from jackdaw, which has the opposite pair
        let configured = room(&found, "jackdaw");
        assert!(configured.configured);
        assert!(!configured.history);
    }

    #[test]
    fn an_org_derived_room_in_no_config_still_appears() {
        let session = tempfile::tempdir().unwrap();
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        let found = rooms(
            &g,
            &[agent_at("/w/acme/web")],
            session.path(),
            &mut cache(fake_org),
        );
        let acme = room(&found, "acme");
        assert_eq!(acme.agents, 1);
        assert!(!acme.configured);
        assert!(!acme.history);
    }

    #[test]
    fn one_room_named_by_all_three_sources_is_listed_once() {
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), Some("alare"), "{}\n");
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        let found = rooms(
            &g,
            &[agent_at("/w/alare/api"), agent_at("/w/alare/web")],
            session.path(),
            &mut cache(fake_org),
        );
        assert_eq!(names(&found), vec!["alare"]);
        assert_eq!(found[0].agents, 2);
        assert!(found[0].configured);
        assert!(found[0].history);
    }

    #[test]
    fn rooms_sort_by_provenance_then_name() {
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), Some("zeta"), "{}\n");
        write_room(session.path(), Some("attic"), "{}\n");
        let g = Grouping::Active(rules(&[
            ("busy", &["/w/busy"]),
            ("also-busy", &["/w/also-busy"]),
            ("quiet", &["/w/quiet"]),
            ("also-quiet", &["/w/also-quiet"]),
        ]));
        let found = rooms(
            &g,
            &[agent_at("/w/busy/x"), agent_at("/w/also-busy/x")],
            session.path(),
            &mut cache(no_org),
        );
        assert_eq!(
            names(&found),
            vec!["also-busy", "busy", "also-quiet", "quiet", "attic", "zeta"]
        );
    }

    #[test]
    fn an_unreadable_session_directory_keeps_the_config_and_the_agents() {
        // the disk sweep is one source of three; losing it must not lose the
        // other two
        let g = Grouping::Active(rules(&[("alare", &["/w/alare"])]));
        let found = rooms(
            &g,
            &[agent_at("/w/acme/web")],
            Path::new("/nonexistent/session/dir"),
            &mut cache(fake_org),
        );
        assert_eq!(names(&found), vec!["acme", "alare"]);
    }

    #[test]
    fn a_subdirectory_that_could_never_be_a_group_is_skipped() {
        // an illegal name can never be passed back as --group, so listing it
        // offers a room nothing can open
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), Some("Not A Group"), "{}\n");
        write_room(session.path(), Some("fine"), "{}\n");
        let found = rooms(&Grouping::Inactive, &[], session.path(), &mut cache(no_org));
        assert_eq!(names(&found), vec!["fine"]);
    }

    #[test]
    fn a_room_lists_every_source_that_vouches_for_it() {
        let r = Room {
            name: Some("alare".into()),
            agents: 2,
            configured: true,
            history: true,
        };
        assert_eq!(
            r.sources(),
            vec![Source::Agents, Source::Config, Source::History]
        );
    }

    #[test]
    fn the_primary_source_of_a_room_with_agents_is_its_agents() {
        // the label and the sort must give one answer about the same room:
        // `alare` sits in the config and holds a log, but agents are standing
        // in it, which is why it sorts first — so it must not read as
        // "known from the config"
        let r = Room {
            name: Some("alare".into()),
            agents: 2,
            configured: true,
            history: true,
        };
        assert_eq!(r.sources().first(), Some(&Source::Agents));
        assert_eq!(r.provenance_rank(), 0);
    }

    #[test]
    fn every_rank_is_its_primary_sources_place_in_provenance_order() {
        let room_with = |agents, configured, history| Room {
            name: None,
            agents,
            configured,
            history,
        };
        assert_eq!(room_with(1, false, false).provenance_rank(), 0);
        assert_eq!(room_with(0, true, true).provenance_rank(), 1);
        assert_eq!(room_with(0, false, true).provenance_rank(), 2);
    }

    #[test]
    fn a_room_nothing_vouches_for_sorts_last_rather_than_first() {
        // `rooms` never builds one, but `Room`'s fields are public and a
        // default-ish room must not outrank a room with agents in it
        let r = room_of_no_sources();
        assert!(r.sources().is_empty());
        assert!(r.provenance_rank() > room_of_history_only().provenance_rank());
    }

    fn room_of_no_sources() -> Room {
        Room {
            name: None,
            agents: 0,
            configured: false,
            history: false,
        }
    }

    fn room_of_history_only() -> Room {
        Room {
            name: None,
            agents: 0,
            configured: false,
            history: true,
        }
    }

    #[test]
    fn the_order_of_a_rooms_sources_matches_the_order_rooms_are_sorted_in() {
        // the invariant that keeps a label and a sort position honest: for
        // any two rooms, the one whose primary source comes first in
        // provenance order sorts first. Written out longhand on purpose —
        // asserting the list is sorted by its own key would hold however
        // that key was declared, and it is the declared order that skews.
        let session = tempfile::tempdir().unwrap();
        write_room(session.path(), Some("zeta"), "{}\n");
        let g = Grouping::Active(rules(&[("busy", &["/w/busy"]), ("quiet", &["/w/quiet"])]));
        let found = rooms(
            &g,
            &[agent_at("/w/busy/x")],
            session.path(),
            &mut cache(no_org),
        );
        let primaries: Vec<Source> = found
            .iter()
            .map(|r| *r.sources().first().unwrap())
            .collect();
        assert_eq!(
            primaries,
            vec![Source::Agents, Source::Config, Source::History]
        );
    }
}
