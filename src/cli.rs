use crate::git_org::OrgCache;
use crate::groups::{self, Grouping};
use crate::herd::{AgentInfo, HerdControl};
use crate::log_store::{self, Message};
use anyhow::{bail, Result};
use std::path::Path;

/// Which room this invocation talks to: a configured prefix, else the repo's
/// origin organization. Refusing an unresolvable cwd under an active config
/// rather than falling back to a default room is deliberate: a silent fallback
/// would show one group's traffic to a caller nothing places there.
pub fn resolve_group(
    explicit: Option<&str>,
    cwd: &Path,
    grouping: &Grouping,
    orgs: &mut OrgCache,
) -> Result<Option<String>> {
    if let Grouping::Broken(msg) = grouping {
        bail!("groups config is unusable: {msg}");
    }
    // Org-derived groups are not enumerable — they exist as soon as an agent
    // with that origin starts — so an explicit name is checked for legality,
    // not membership. A typo therefore opens an empty room rather than
    // erroring; the TUI titles every room, which is where that shows up.
    if let Some(g) = explicit {
        if !groups::valid_group_name(g) {
            bail!("invalid group name {g:?} (allowed: [a-z0-9][a-z0-9_-]*)");
        }
        return Ok(Some(g.to_string()));
    }
    match groups::resolve(cwd, grouping, orgs) {
        Some(g) => Ok(Some(g)),
        // No config and no repo is v1's single shared room, not an error.
        None if matches!(grouping, Grouping::Inactive) => Ok(None),
        None => bail!(
            "cwd {} matches no group and its repo has no origin remote; \
             add a prefix for it to groups.toml or pass --group",
            cwd.display()
        ),
    }
}

fn current_grouping() -> Result<Grouping> {
    Ok(groups::load(&crate::paths::base_dir()?))
}

/// Resolves the group for a CLI invocation from the process's own cwd. The
/// `Grouping` and the org cache come back with it because callers that list
/// agents must scope the listing to the resolved group, which needs both.
fn group_for_invocation(explicit: Option<&str>) -> Result<(Option<String>, Grouping, OrgCache)> {
    let cwd = std::env::current_dir()?;
    let grouping = current_grouping()?;
    let mut orgs = OrgCache::default();
    let resolved = resolve_group(explicit, &cwd, &grouping, &mut orgs)?;
    Ok((resolved, grouping, orgs))
}

/// The agents a caller in `resolved` may see. The roster is always scoped to
/// the room: herdr agent names routinely encode client and issue identity, so
/// an unscoped listing leaks one company's work to another's agent. The
/// ungrouped room (`resolved` is `None`, no config) is a room too, holding the
/// agents that belong to no repository.
pub fn visible_agents<'a>(
    agents: &'a [AgentInfo],
    resolved: Option<&str>,
    grouping: &Grouping,
    orgs: &mut OrgCache,
) -> Vec<&'a AgentInfo> {
    match grouping {
        Grouping::Broken(_) => Vec::new(),
        // Under a config, "no group" means "enrolled nowhere", so it has no
        // roster of its own.
        Grouping::Active(_) if resolved.is_none() => Vec::new(),
        _ => agents
            .iter()
            .filter(|a| groups::resolve(Path::new(&a.cwd), grouping, orgs).as_deref() == resolved)
            .collect(),
    }
}

pub fn cmd_groups(herd: &dyn HerdControl) -> Result<()> {
    let grouping = current_grouping()?;
    if let Grouping::Broken(msg) = &grouping {
        println!("groups config BROKEN — no agent is enrolled: {msg}");
        return Ok(());
    }
    match &grouping {
        Grouping::Inactive => {
            println!("no groups.toml — each agent's group is its repository's origin organization")
        }
        _ => println!("groups.toml rules first, then each agent's repository origin"),
    }
    let agents = match herd.list_agents() {
        Ok(a) => a,
        Err(e) => {
            // Distinguish "nobody is enrolled" from "I could not ask": this is
            // the auditing surface, and an empty roster that really means herdr
            // is down must not read as isolation.
            println!(
                "cannot list agents ({e}) — membership below is unknown; \
                 only the configured prefixes are shown"
            );
            Vec::new()
        }
    };
    let mut orgs = OrgCache::default();
    let mut members: std::collections::BTreeMap<Option<String>, Vec<&AgentInfo>> =
        std::collections::BTreeMap::new();
    for a in &agents {
        members
            .entry(groups::resolve(Path::new(&a.cwd), &grouping, &mut orgs))
            .or_default()
            .push(a);
    }
    let print_members =
        |name: &Option<String>,
         members: &std::collections::BTreeMap<Option<String>, Vec<&AgentInfo>>| {
            for a in members.get(name).unwrap_or(&Vec::new()) {
                println!("  {}\t{}\t{}", a.name, a.status, a.cwd);
            }
        };
    let mut configured: Vec<String> = Vec::new();
    if let Grouping::Active(rules) = &grouping {
        for name in rules.names() {
            let paths: Vec<String> = rules
                .prefixes_for(name)
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            println!("{name}\t{}", paths.join(" "));
            print_members(&Some(name.to_string()), &members);
            configured.push(name.to_string());
        }
    }
    // Groups nothing in the config names: they exist because an agent's repo
    // points at that organization, and they appear and vanish with the agents.
    for name in members.keys().flatten() {
        if configured.iter().any(|c| c == name) {
            continue;
        }
        println!("{name}\t(from repo origin)");
        print_members(&Some(name.clone()), &members);
    }
    if let Some(ungrouped) = members.get(&None) {
        match grouping {
            Grouping::Inactive => println!("ungrouped (shared room — no repository origin)"),
            _ => println!("ungrouped (receiving nothing)"),
        }
        for a in ungrouped {
            println!("  {}\t{}\t{}", a.name, a.status, a.cwd);
        }
    }
    Ok(())
}

pub fn resolve_sender(
    as_flag: Option<&str>,
    pane_env: Option<&str>,
    agents: &[AgentInfo],
) -> Result<String> {
    if let Some(name) = as_flag {
        return Ok(name.to_string());
    }
    if let Some(pane) = pane_env {
        if let Some(a) = agents.iter().find(|a| a.pane_id == pane) {
            return Ok(a.name.clone());
        }
    }
    bail!(
        "cannot determine sender: pass --as <name>, or run from a pane \
         hosting a named herdr agent"
    );
}

pub fn format_messages(msgs: &[Message]) -> String {
    msgs.iter()
        .map(|m| format!("[#{} {}] {}: {}\n", m.id, m.ts, m.from, m.text))
        .collect()
}

pub fn cmd_post(
    group: Option<&str>,
    herd: &dyn HerdControl,
    as_flag: Option<&str>,
    text: &str,
) -> Result<()> {
    let (resolved, _, _) = group_for_invocation(group)?;
    let dir = crate::paths::room_dir(resolved.as_deref())?;
    let pane = std::env::var("HERDR_PANE_ID").ok();
    // Only hit the herd (a live `herdr agent list` call) when we actually need
    // it to resolve a pane -> agent name. `--as` fully determines the sender,
    // so skip the lookup then; this keeps `post --as human` working even when
    // herdr is unreachable.
    let agents = if as_flag.is_none() {
        herd.list_agents()?
    } else {
        Vec::new()
    };
    let sender = resolve_sender(as_flag, pane.as_deref(), &agents)?;
    let msg = log_store::append(&dir, &sender, text)?;
    println!("posted #{}", msg.id);
    Ok(())
}

pub fn cmd_read(group: Option<&str>, since: Option<u64>, limit: usize) -> Result<()> {
    let (resolved, _, _) = group_for_invocation(group)?;
    let dir = crate::paths::room_dir(resolved.as_deref())?;
    let mut msgs = log_store::read_since(&dir, since.unwrap_or(0))?;
    if since.is_none() && msgs.len() > limit {
        msgs = msgs.split_off(msgs.len() - limit);
    }
    print!("{}", format_messages(&msgs));
    Ok(())
}

pub fn cmd_agents(group: Option<&str>, herd: &dyn HerdControl) -> Result<()> {
    let (resolved, grouping, mut orgs) = group_for_invocation(group)?;
    let agents = herd.list_agents()?;
    for a in visible_agents(&agents, resolved.as_deref(), &grouping, &mut orgs) {
        println!("{}\t{}\t{}", a.name, a.status, a.pane_id);
    }
    println!("human\t-\t-");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::Grouping;
    use crate::herd::AgentInfo;
    use crate::log_store::Message;

    fn active() -> Grouping {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groups.toml"),
            "[groups]\nalare = [\"/w/alare\"]\nacme = [\"/w/acme\"]\n",
        )
        .unwrap();
        let g = crate::groups::load(dir.path());
        std::mem::forget(dir);
        g
    }

    fn no_org(_cwd: &Path) -> Option<String> {
        None
    }

    /// `/w/<org>/...` belongs to `<org>`; anything else is outside a repo.
    fn fake_org(cwd: &Path) -> Option<String> {
        let s = cwd.to_string_lossy();
        let rest = s.strip_prefix("/w/")?;
        Some(rest.split('/').next()?.to_string())
    }

    fn orgs(lookup: fn(&Path) -> Option<String>) -> crate::git_org::OrgCache {
        crate::git_org::OrgCache::with_lookup(lookup, std::time::Duration::from_secs(300))
    }

    #[test]
    fn inactive_grouping_resolves_from_the_repo_org() {
        let g = resolve_group(
            None,
            Path::new("/w/beta/api"),
            &Grouping::Inactive,
            &mut orgs(fake_org),
        )
        .unwrap();
        assert_eq!(g.as_deref(), Some("beta"));
    }

    #[test]
    fn inactive_grouping_outside_a_repo_is_the_shared_room() {
        let g = resolve_group(
            None,
            Path::new("/anywhere"),
            &Grouping::Inactive,
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(g, None);
    }

    #[test]
    fn explicit_group_is_allowed_without_a_config() {
        // org groups are not enumerable, so there is no list to check against
        let g = resolve_group(
            Some("acme"),
            Path::new("/anywhere"),
            &Grouping::Inactive,
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(g.as_deref(), Some("acme"));
    }

    #[test]
    fn an_unusable_explicit_group_name_is_an_error() {
        // a name no room can ever have is a typo, not a room to open
        assert!(resolve_group(
            Some("Acme Corp!"),
            Path::new("/anywhere"),
            &Grouping::Inactive,
            &mut orgs(no_org)
        )
        .is_err());
    }

    #[test]
    fn active_grouping_resolves_from_cwd() {
        let g = resolve_group(
            None,
            Path::new("/w/alare/api"),
            &active(),
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(g.as_deref(), Some("alare"));
    }

    #[test]
    fn explicit_group_overrides_cwd() {
        let g = resolve_group(
            Some("acme"),
            Path::new("/w/alare/api"),
            &active(),
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(g.as_deref(), Some("acme"));
    }

    #[test]
    fn an_unmatched_cwd_resolves_to_its_repo_org() {
        let g = resolve_group(
            None,
            Path::new("/w/beta/api"),
            &active(),
            &mut orgs(fake_org),
        )
        .unwrap();
        assert_eq!(g.as_deref(), Some("beta"));
    }

    #[test]
    fn ungrouped_cwd_outside_a_repo_is_refused_not_defaulted() {
        let err = resolve_group(
            None,
            Path::new("/tmp/scratch"),
            &active(),
            &mut orgs(no_org),
        )
        .unwrap_err();
        assert!(err.to_string().contains("/tmp/scratch"));
    }

    #[test]
    fn broken_config_is_refused() {
        assert!(resolve_group(
            None,
            Path::new("/w/alare"),
            &Grouping::Broken("bad".into()),
            &mut orgs(fake_org)
        )
        .is_err());
    }

    fn agents() -> Vec<AgentInfo> {
        vec![AgentInfo {
            name: "reviewer".into(),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
            cwd: String::new(),
        }]
    }

    fn grouped_agents() -> Vec<AgentInfo> {
        vec![
            AgentInfo {
                name: "issue-590".into(),
                pane_id: "w1:p1".into(),
                status: "idle".into(),
                cwd: "/w/alare/api".into(),
            },
            AgentInfo {
                name: "acme-secret-issue".into(),
                pane_id: "w2:p1".into(),
                status: "idle".into(),
                cwd: "/w/acme/web".into(),
            },
            AgentInfo {
                name: "stray".into(),
                pane_id: "w3:p1".into(),
                status: "idle".into(),
                cwd: "/tmp/scratch".into(),
            },
        ]
    }

    #[test]
    fn listing_is_scoped_to_the_callers_group() {
        let all = grouped_agents();
        let seen: Vec<&str> = visible_agents(&all, Some("alare"), &active(), &mut orgs(no_org))
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(seen, vec!["issue-590"]);
    }

    #[test]
    fn listing_is_scoped_to_the_org_room_too() {
        // an org room is a room like any other: its roster must not show the
        // agents of the group next door
        let all = grouped_agents();
        let seen: Vec<&str> = visible_agents(&all, Some("acme"), &active(), &mut orgs(fake_org))
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(seen, vec!["acme-secret-issue"]);
    }

    #[test]
    fn the_shared_room_roster_holds_only_agents_outside_a_repo() {
        let all = grouped_agents();
        let seen: Vec<&str> = visible_agents(&all, None, &Grouping::Inactive, &mut orgs(fake_org))
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(seen, vec!["stray"]);
    }

    #[test]
    fn an_org_room_roster_is_scoped_without_a_config() {
        let all = grouped_agents();
        let seen: Vec<&str> = visible_agents(
            &all,
            Some("alare"),
            &Grouping::Inactive,
            &mut orgs(fake_org),
        )
        .iter()
        .map(|a| a.name.as_str())
        .collect();
        assert_eq!(seen, vec!["issue-590"]);
    }

    #[test]
    fn as_flag_wins() {
        assert_eq!(
            resolve_sender(Some("human"), Some("w1:p1"), &agents()).unwrap(),
            "human"
        );
    }

    #[test]
    fn pane_env_resolves_to_agent_name() {
        assert_eq!(
            resolve_sender(None, Some("w1:p1"), &agents()).unwrap(),
            "reviewer"
        );
    }

    #[test]
    fn unknown_pane_is_an_error() {
        assert!(resolve_sender(None, Some("w9:p9"), &agents()).is_err());
        assert!(resolve_sender(None, None, &agents()).is_err());
    }

    #[test]
    fn formats_messages() {
        let msgs = vec![Message {
            id: 3,
            ts: "2026-08-18T12:00:00+00:00".into(),
            from: "reviewer".into(),
            text: "done".into(),
        }];
        assert_eq!(
            format_messages(&msgs),
            "[#3 2026-08-18T12:00:00+00:00] reviewer: done\n"
        );
    }
}
