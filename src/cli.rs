use crate::git_org::OrgCache;
use crate::groups::{self, CurrentRoom, Grouping};
use crate::herd::{AgentInfo, HerdControl};
use crate::log_store::{self, Message};
use anyhow::{bail, Result};
use std::fmt::Write as _;
use std::path::Path;

/// Which room this invocation talks to: a configured prefix, else the repo's
/// origin organization. Refusing an unresolvable cwd under an active config
/// rather than falling back to a default room is deliberate: a silent fallback
/// would show one group's traffic to a caller nothing places there.
/// The room a caller lands in, as the three-state room a chat pane holds.
///
/// `Broken` still bails: `groups::rooms` lists nothing there, so the picker
/// this feeds could only offer rooms swept off disk — every company's — from
/// a config we could not parse.
///
/// An active config that matches nothing is `NoneSelected` rather than an
/// error. Only a chat pane can use that state, which is why `resolve_group`
/// below still refuses it for every one-shot command.
pub fn resolve_room(
    explicit: Option<&str>,
    cwd: &Path,
    grouping: &Grouping,
    orgs: &mut OrgCache,
) -> Result<CurrentRoom> {
    if let Grouping::Broken(msg) = grouping {
        bail!("groups config is unusable: {msg}");
    }
    // There is no roster of legal group names to check against. `rooms`
    // enumerates the rooms something vouches for — an agent standing in one,
    // the config, a log on disk — but an org-derived group exists the moment
    // an agent with that origin starts, so a group with neither is legal and
    // absent from that list. An explicit name is therefore checked for
    // legality, not membership. A typo opens an empty room rather than
    // erroring; the TUI titles every room, which is where that shows up.
    if let Some(g) = explicit {
        if !groups::valid_group_name(g) {
            bail!("invalid group name {g:?} (allowed: [a-z0-9][a-z0-9_-]*)");
        }
        return Ok(CurrentRoom::Named(g.to_string()));
    }
    Ok(match groups::resolve(cwd, grouping, orgs) {
        Some(g) => CurrentRoom::Named(g),
        // No config and no repo is v1's single shared room, not an error.
        None if matches!(grouping, Grouping::Inactive) => CurrentRoom::Ungrouped,
        None => CurrentRoom::NoneSelected,
    })
}

/// The group a one-shot command operates on. Everything but the chat pane
/// needs a room right now, so the state a pane opens a picker in is an error
/// here. Delegates to `resolve_room` so the precedence — an explicit name,
/// then a configured prefix, then the repo's origin — is written once.
pub fn resolve_group(
    explicit: Option<&str>,
    cwd: &Path,
    grouping: &Grouping,
    orgs: &mut OrgCache,
) -> Result<Option<String>> {
    match resolve_room(explicit, cwd, grouping, orgs)? {
        CurrentRoom::Named(g) => Ok(Some(g)),
        CurrentRoom::Ungrouped => Ok(None),
        CurrentRoom::NoneSelected => bail!(
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
    // Not asked for under `Broken`: the listing names no agent there, so a
    // round trip to herdr could only produce a roster we would not print.
    let agents = match grouping {
        Grouping::Broken(_) => Ok(Vec::new()),
        _ => herd.list_agents().map_err(|e| e.to_string()),
    };
    let mut orgs = OrgCache::default();
    print!(
        "{}",
        render_groups(
            &grouping,
            agents.as_deref().map_err(String::as_str),
            &mut orgs
        )
    );
    Ok(())
}

/// The `groups` listing as text, so every shape of it can be asserted on.
/// `current_grouping` reads a fixed path and the roster comes from a live
/// herdr, so the three shapes that carry the most logic — a group named by no
/// config, agents the config enrolls nowhere, and a herd that will not answer
/// — are unreachable through `cmd_groups` itself. The real config on this
/// machine produces none of them.
///
/// `agents` carries the listing failure rather than an empty roster because
/// the two print differently: telling "nobody is enrolled" apart from "I could
/// not ask" is what this auditing surface is for.
fn render_groups(
    grouping: &Grouping,
    agents: std::result::Result<&[AgentInfo], &str>,
    orgs: &mut OrgCache,
) -> String {
    // Writing to a String cannot fail, hence the discarded results throughout.
    let mut out = String::new();
    if let Grouping::Broken(msg) = grouping {
        let _ = writeln!(out, "groups config BROKEN — no agent is enrolled: {msg}");
        return out;
    }
    let _ = match grouping {
        Grouping::Inactive => writeln!(
            out,
            "no groups.toml — each agent's group is its repository's origin organization"
        ),
        _ => writeln!(
            out,
            "groups.toml rules first, then each agent's repository origin"
        ),
    };
    let agents: &[AgentInfo] = match agents {
        Ok(a) => a,
        Err(e) => {
            // Distinguish "nobody is enrolled" from "I could not ask": this is
            // the auditing surface, and an empty roster that really means herdr
            // is down must not read as isolation.
            let _ = writeln!(
                out,
                "cannot list agents ({e}) — membership below is unknown; \
                 only the configured prefixes are shown"
            );
            &[]
        }
    };
    let mut members: std::collections::BTreeMap<Option<String>, Vec<&AgentInfo>> =
        std::collections::BTreeMap::new();
    for a in agents {
        members
            .entry(groups::resolve(Path::new(&a.cwd), grouping, orgs))
            .or_default()
            .push(a);
    }
    let print_members =
        |out: &mut String,
         name: &Option<String>,
         members: &std::collections::BTreeMap<Option<String>, Vec<&AgentInfo>>| {
            for a in members.get(name).unwrap_or(&Vec::new()) {
                let _ = writeln!(out, "  {}\t{}\t{}", a.name, a.status, a.cwd);
            }
        };
    let mut configured: Vec<String> = Vec::new();
    if let Grouping::Active(rules) = grouping {
        for name in rules.names() {
            let paths: Vec<String> = rules
                .prefixes_for(name)
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let _ = writeln!(out, "{name}\t{}", paths.join(" "));
            print_members(&mut out, &Some(name.to_string()), &members);
            configured.push(name.to_string());
        }
    }
    // Groups nothing in the config names: they exist because an agent's repo
    // points at that organization, and they appear and vanish with the agents.
    for name in members.keys().flatten() {
        if configured.iter().any(|c| c == name) {
            continue;
        }
        let _ = writeln!(out, "{name}\t(from repo origin)");
        print_members(&mut out, &Some(name.clone()), &members);
    }
    if let Some(ungrouped) = members.get(&None) {
        let _ = match grouping {
            Grouping::Inactive => {
                writeln!(out, "ungrouped (shared room — no repository origin)")
            }
            _ => writeln!(out, "ungrouped (receiving nothing)"),
        };
        for a in ungrouped {
            let _ = writeln!(out, "  {}\t{}\t{}", a.name, a.status, a.cwd);
        }
    }
    out
}

/// The rooms this session could open. Not scoped to the caller's group:
/// this is an enumeration surface like `groups`, which already prints every
/// group from any cwd, and it deliberately does not resolve the caller's own
/// cwd — the one place you most want to run it is a cwd that resolves to
/// nothing, where `resolve_group` would bail.
pub fn cmd_rooms(herd: &dyn HerdControl) -> Result<()> {
    let grouping = current_grouping()?;
    if let Grouping::Broken(msg) = &grouping {
        // `rooms` returns nothing here, and silence would read as "no rooms
        // exist" rather than "I could not tell".
        println!("groups config BROKEN — no room is listed: {msg}");
        return Ok(());
    }
    let agents = match herd.list_agents() {
        Ok(a) => a,
        Err(e) => {
            // Without a roster every room reads as quiet, which sorts live
            // rooms down among the dead ones. Say so rather than let the
            // order lie.
            println!(
                "cannot list agents ({e}) — every room below reads as having none, \
                 whether or not it does"
            );
            Vec::new()
        }
    };
    let session = crate::paths::session_dir()?;
    let mut orgs = OrgCache::default();
    println!("name\tagents\tknown from");
    for r in groups::rooms(&grouping, &agents, &session, &mut orgs) {
        // Straight from `Room::sources`, which `rooms` also sorts by, so
        // this column cannot disagree with the order it is printed in.
        let from: Vec<&str> = r.sources().iter().map(|s| s.label()).collect();
        println!(
            "{}\t{}\t{}",
            r.name.as_deref().unwrap_or("(ungrouped)"),
            r.agents,
            from.join(", ")
        );
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

/// Ceiling on a posted message, in Unicode scalar values. 700 leaves about
/// 30% headroom over the longest message the room judged worth its length,
/// while rejecting 90 of the 99 messages measured in #8. ADR-0001 records
/// why the guidance is a rejected command rather than a sentence asking for
/// brevity.
pub const MAX_POST_CHARS: usize = 700;

pub fn cmd_post(
    group: Option<&str>,
    herd: &dyn HerdControl,
    as_flag: Option<&str>,
    text: &str,
) -> Result<()> {
    // First, before resolving anything. The length depends on the text alone,
    // so checking here is what makes `--as human` capped without a special
    // case, and what stops an unresolvable cwd or an unreachable herd from
    // reporting itself instead of the length.
    let chars = text.chars().count();
    if chars > MAX_POST_CHARS {
        bail!("message is {chars} chars; limit is {MAX_POST_CHARS}. Post a summary under {MAX_POST_CHARS} chars and put the detail on the issue.");
    }
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
        // org groups appear as their agents start, so there is no roster of
        // legal names to check an explicit one against
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

    #[test]
    fn a_chat_pane_opens_with_no_room_where_a_command_would_be_refused() {
        // The same cwd `ungrouped_cwd_outside_a_repo_is_refused_not_defaulted`
        // rejects. A pane can offer a picker there; a one-shot command cannot.
        let room = resolve_room(
            None,
            Path::new("/tmp/scratch"),
            &active(),
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(room, CurrentRoom::NoneSelected);
    }

    #[test]
    fn no_config_and_no_repo_is_the_shared_room_not_an_absent_one() {
        let room = resolve_room(
            None,
            Path::new("/tmp/scratch"),
            &Grouping::Inactive,
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(room, CurrentRoom::Ungrouped);
    }

    #[test]
    fn a_broken_config_is_refused_a_room_even_for_a_pane() {
        // `groups::rooms` lists nothing under `Broken`, so the picker this
        // would open could only offer rooms swept off disk — every
        // company's — from a config we could not parse.
        assert!(resolve_room(
            None,
            Path::new("/w/alare"),
            &Grouping::Broken("bad".into()),
            &mut orgs(fake_org)
        )
        .is_err());
    }

    #[test]
    fn an_explicit_group_names_the_room_for_a_pane_too() {
        let room = resolve_room(
            Some("acme"),
            Path::new("/tmp/scratch"),
            &active(),
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(room, CurrentRoom::Named("acme".into()));
    }

    fn agents() -> Vec<AgentInfo> {
        vec![AgentInfo {
            name: "reviewer".into(),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
            cwd: String::new(),
            focused: Some(false),
            session: None,
        }]
    }

    fn grouped_agents() -> Vec<AgentInfo> {
        vec![
            AgentInfo {
                name: "issue-590".into(),
                pane_id: "w1:p1".into(),
                status: "idle".into(),
                cwd: "/w/alare/api".into(),
                focused: Some(false),
                session: None,
            },
            AgentInfo {
                name: "acme-secret-issue".into(),
                pane_id: "w2:p1".into(),
                status: "idle".into(),
                cwd: "/w/acme/web".into(),
                focused: Some(false),
                session: None,
            },
            AgentInfo {
                name: "stray".into(),
                pane_id: "w3:p1".into(),
                status: "idle".into(),
                cwd: "/tmp/scratch".into(),
                focused: Some(false),
                session: None,
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

    /// `cmd_post` with `--as` never reaches the herd; a stub that panics
    /// proves the length check runs before anything else is resolved.
    struct NoHerd;
    impl HerdControl for NoHerd {
        fn list_agents(&self) -> Result<Vec<AgentInfo>> {
            panic!("cmd_post must not consult the herd");
        }
        fn prompt(&self, _name: &str, _text: &str) -> Result<crate::herd::Delivery> {
            panic!("cmd_post must not prompt");
        }
    }

    /// Points every path helper at a fresh room. The caller holds
    /// `paths::env_guard` and must keep the returned dir alive.
    fn room() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SCUTTLEBUTT_DIR", dir.path());
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/cap-test.sock");
        dir
    }

    fn unset_room() {
        std::env::remove_var("SCUTTLEBUTT_DIR");
        std::env::remove_var("HERDR_SOCKET_PATH");
    }

    /// Derived through the same helper the command uses, so the negative
    /// assertions below cannot pass by pointing at a path nothing writes.
    fn room_file() -> std::path::PathBuf {
        crate::paths::room_dir(Some("t"))
            .unwrap()
            .join("room.jsonl")
    }

    #[test]
    fn a_message_at_the_limit_is_posted() {
        let _env = crate::paths::env_guard();
        let _dir = room();
        let text = "a".repeat(MAX_POST_CHARS);
        cmd_post(Some("t"), &NoHerd, Some("agent"), &text).unwrap();
        let msgs = log_store::read_since(room_file().parent().unwrap(), 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text.chars().count(), MAX_POST_CHARS);
        unset_room();
    }

    #[test]
    fn one_char_over_the_limit_is_refused_and_leaves_the_log_untouched() {
        let _env = crate::paths::env_guard();
        let _dir = room();
        cmd_post(Some("t"), &NoHerd, Some("agent"), "already here").unwrap();
        let before = std::fs::read(room_file()).unwrap();

        let text = "a".repeat(MAX_POST_CHARS + 1);
        let err = cmd_post(Some("t"), &NoHerd, Some("agent"), &text).unwrap_err();
        assert_eq!(
            err.to_string(),
            "message is 701 chars; limit is 700. Post a summary under 700 chars and put the detail on the issue."
        );
        assert_eq!(std::fs::read(room_file()).unwrap(), before);
        unset_room();
    }

    #[test]
    fn a_rejected_first_post_does_not_even_create_the_log() {
        // `append` opens with `create(true)`, so a file that never appears is
        // what proves the check ran before the write rather than after it.
        let _env = crate::paths::env_guard();
        let _dir = room();
        let text = "a".repeat(MAX_POST_CHARS + 1);
        assert!(cmd_post(Some("t"), &NoHerd, Some("agent"), &text).is_err());
        assert!(!room_file().exists());
        unset_room();
    }

    #[test]
    fn the_limit_counts_characters_not_bytes() {
        // A 700-character message of 4-byte characters is 2800 bytes; a byte
        // count would reject it.
        let _env = crate::paths::env_guard();
        let _dir = room();
        let text = "\u{1F600}".repeat(MAX_POST_CHARS);
        cmd_post(Some("t"), &NoHerd, Some("agent"), &text).unwrap();
        let msgs = log_store::read_since(room_file().parent().unwrap(), 0).unwrap();
        assert_eq!(msgs[0].text.chars().count(), MAX_POST_CHARS);
        unset_room();
    }

    #[test]
    fn posting_as_human_is_capped_too() {
        // The obvious evasion: the human's TUI is uncapped, so the command
        // must not become uncapped by claiming to be the human.
        let _env = crate::paths::env_guard();
        let _dir = room();
        let text = "a".repeat(MAX_POST_CHARS + 1);
        assert!(cmd_post(Some("t"), &NoHerd, Some("human"), &text).is_err());
        assert!(!room_file().exists());
        unset_room();
    }

    #[test]
    fn the_tui_append_path_is_uncapped() {
        // The human posts by appending directly; keeping the check out of
        // `log_store::append` is what preserves that.
        let dir = tempfile::tempdir().unwrap();
        let text = "a".repeat(MAX_POST_CHARS + 500);
        log_store::append(dir.path(), "human", &text).unwrap();
        let msgs = log_store::read_since(dir.path(), 0).unwrap();
        assert_eq!(msgs[0].text.chars().count(), MAX_POST_CHARS + 500);
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

    /// Adds the shapes the real config cannot produce: an agent whose repo
    /// origin names a group no config does, and an agent under no repo at all.
    fn mixed_agents() -> Vec<AgentInfo> {
        let mut v = grouped_agents();
        v.push(AgentInfo {
            name: "beta-1".into(),
            pane_id: "w4:p1".into(),
            status: "working".into(),
            cwd: "/w/beta/api".into(),
            focused: Some(false),
            session: None,
        });
        v
    }

    #[test]
    fn a_broken_config_says_so_rather_than_listing_nothing() {
        let out = render_groups(
            &Grouping::Broken("bad toml".into()),
            Ok(&mixed_agents()),
            &mut orgs(fake_org),
        );
        assert_eq!(
            out,
            "groups config BROKEN — no agent is enrolled: bad toml\n"
        );
    }

    #[test]
    fn configured_groups_come_before_org_derived_ones_with_their_members() {
        let out = render_groups(&active(), Ok(&mixed_agents()), &mut orgs(fake_org));
        assert_eq!(
            out,
            "groups.toml rules first, then each agent's repository origin\n\
             acme\t/w/acme\n  \
             acme-secret-issue\tidle\t/w/acme/web\n\
             alare\t/w/alare\n  \
             issue-590\tidle\t/w/alare/api\n\
             beta\t(from repo origin)\n  \
             beta-1\tworking\t/w/beta/api\n\
             ungrouped (receiving nothing)\n  \
             stray\tidle\t/tmp/scratch\n"
        );
    }

    #[test]
    fn without_a_config_every_group_is_an_org_and_ungrouped_is_the_shared_room() {
        let out = render_groups(
            &Grouping::Inactive,
            Ok(&mixed_agents()),
            &mut orgs(fake_org),
        );
        assert_eq!(
            out,
            "no groups.toml — each agent's group is its repository's origin organization\n\
             acme\t(from repo origin)\n  \
             acme-secret-issue\tidle\t/w/acme/web\n\
             alare\t(from repo origin)\n  \
             issue-590\tidle\t/w/alare/api\n\
             beta\t(from repo origin)\n  \
             beta-1\tworking\t/w/beta/api\n\
             ungrouped (shared room — no repository origin)\n  \
             stray\tidle\t/tmp/scratch\n"
        );
    }

    #[test]
    fn a_herd_that_will_not_answer_still_prints_the_configured_prefixes() {
        let out = render_groups(&active(), Err("herdr not running"), &mut orgs(fake_org));
        assert_eq!(
            out,
            "groups.toml rules first, then each agent's repository origin\n\
             cannot list agents (herdr not running) — membership below is unknown; \
             only the configured prefixes are shown\n\
             acme\t/w/acme\n\
             alare\t/w/alare\n"
        );
    }
}
