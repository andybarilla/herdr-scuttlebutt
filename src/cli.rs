use crate::herd::{AgentInfo, HerdControl};
use crate::log_store::{self, Message};
use anyhow::{bail, Result};

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

pub fn cmd_post(herd: &dyn HerdControl, as_flag: Option<&str>, text: &str) -> Result<()> {
    let dir = crate::paths::room_dir()?;
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

pub fn cmd_read(since: Option<u64>, limit: usize) -> Result<()> {
    let dir = crate::paths::room_dir()?;
    let mut msgs = log_store::read_since(&dir, since.unwrap_or(0))?;
    if since.is_none() && msgs.len() > limit {
        msgs = msgs.split_off(msgs.len() - limit);
    }
    print!("{}", format_messages(&msgs));
    Ok(())
}

pub fn cmd_agents(herd: &dyn HerdControl) -> Result<()> {
    for a in herd.list_agents()? {
        println!("{}\t{}\t{}", a.name, a.status, a.pane_id);
    }
    println!("human\t-\t-");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herd::AgentInfo;
    use crate::log_store::Message;

    fn agents() -> Vec<AgentInfo> {
        vec![AgentInfo {
            name: "reviewer".into(),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
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
