use crate::groups::{self, Grouping};
use crate::herd::{AgentInfo, HerdControl};
use crate::log_store::{self, Message};
use anyhow::{bail, Result};
use std::path::Path;

/// Which room this invocation talks to. Refusing an ungrouped cwd rather than
/// falling back to a default room is deliberate: a silent fallback would show
/// one group's traffic to a caller the config does not place there.
pub fn resolve_group(
    explicit: Option<&str>,
    cwd: &Path,
    grouping: &Grouping,
) -> Result<Option<String>> {
    match grouping {
        Grouping::Broken(msg) => bail!("groups config is unusable: {msg}"),
        Grouping::Inactive => {
            if let Some(g) = explicit {
                bail!("--group {g} given but no groups.toml exists");
            }
            Ok(None)
        }
        Grouping::Active(rules) => {
            if let Some(g) = explicit {
                if !rules.names().contains(&g) {
                    bail!(
                        "unknown group {g:?}; configured groups: {}",
                        rules.names().join(", ")
                    );
                }
                return Ok(Some(g.to_string()));
            }
            match groups::group_for(cwd, rules) {
                Some(g) => Ok(Some(g.to_string())),
                None => bail!(
                    "cwd {} matches no group; add a prefix for it to groups.toml \
                     or pass --group",
                    cwd.display()
                ),
            }
        }
    }
}

fn current_grouping() -> Result<Grouping> {
    Ok(groups::load(&crate::paths::base_dir()?))
}

/// Resolves the group for a CLI invocation from the process's own cwd.
fn group_for_invocation(explicit: Option<&str>) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    resolve_group(explicit, &cwd, &current_grouping()?)
}

pub fn cmd_groups(herd: &dyn HerdControl) -> Result<()> {
    match current_grouping()? {
        Grouping::Inactive => {
            println!("grouping inactive (no groups.toml) — one room for all agents");
        }
        Grouping::Broken(msg) => {
            println!("groups config BROKEN — no agent is enrolled: {msg}");
        }
        Grouping::Active(rules) => {
            let agents = herd.list_agents().unwrap_or_default();
            for name in rules.names() {
                let paths: Vec<String> = rules
                    .prefixes_for(name)
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                println!("{name}\t{}", paths.join(" "));
                for a in &agents {
                    if groups::group_for(Path::new(&a.cwd), &rules) == Some(name) {
                        println!("  {}\t{}\t{}", a.name, a.status, a.cwd);
                    }
                }
            }
            let ungrouped: Vec<&AgentInfo> = agents
                .iter()
                .filter(|a| groups::group_for(Path::new(&a.cwd), &rules).is_none())
                .collect();
            if !ungrouped.is_empty() {
                println!("ungrouped (receiving nothing)");
                for a in ungrouped {
                    println!("  {}\t{}\t{}", a.name, a.status, a.cwd);
                }
            }
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
    let resolved = group_for_invocation(group)?;
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
    let resolved = group_for_invocation(group)?;
    let dir = crate::paths::room_dir(resolved.as_deref())?;
    let mut msgs = log_store::read_since(&dir, since.unwrap_or(0))?;
    if since.is_none() && msgs.len() > limit {
        msgs = msgs.split_off(msgs.len() - limit);
    }
    print!("{}", format_messages(&msgs));
    Ok(())
}

pub fn cmd_agents(group: Option<&str>, herd: &dyn HerdControl) -> Result<()> {
    let resolved = group_for_invocation(group)?;
    crate::paths::room_dir(resolved.as_deref())?;
    for a in herd.list_agents()? {
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

    #[test]
    fn inactive_grouping_resolves_to_no_group() {
        let g = resolve_group(None, Path::new("/anywhere"), &Grouping::Inactive).unwrap();
        assert_eq!(g, None);
    }

    #[test]
    fn active_grouping_resolves_from_cwd() {
        let g = resolve_group(None, Path::new("/w/alare/api"), &active()).unwrap();
        assert_eq!(g.as_deref(), Some("alare"));
    }

    #[test]
    fn explicit_group_overrides_cwd() {
        let g = resolve_group(Some("acme"), Path::new("/w/alare/api"), &active()).unwrap();
        assert_eq!(g.as_deref(), Some("acme"));
    }

    #[test]
    fn unknown_explicit_group_is_an_error() {
        assert!(resolve_group(Some("nope"), Path::new("/w/alare"), &active()).is_err());
    }

    #[test]
    fn ungrouped_cwd_is_refused_not_defaulted() {
        let err = resolve_group(None, Path::new("/tmp/scratch"), &active()).unwrap_err();
        assert!(err.to_string().contains("/tmp/scratch"));
    }

    #[test]
    fn broken_config_is_refused() {
        assert!(
            resolve_group(None, Path::new("/w/alare"), &Grouping::Broken("bad".into())).is_err()
        );
    }

    fn agents() -> Vec<AgentInfo> {
        vec![AgentInfo {
            name: "reviewer".into(),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
            cwd: String::new(),
        }]
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
