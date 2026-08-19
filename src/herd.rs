use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub name: String,
    pub pane_id: String,
    pub status: String,
    pub cwd: String,
}

pub trait HerdControl {
    fn list_agents(&self) -> Result<Vec<AgentInfo>>;
    fn prompt(&self, name: &str, text: &str) -> Result<()>;
}

pub fn parse_agent_list(json: &str) -> Result<Vec<AgentInfo>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing agent list JSON")?;
    let agents = v["result"]["agents"]
        .as_array()
        .context("missing .result.agents")?;
    Ok(agents
        .iter()
        .filter_map(|a| {
            Some(AgentInfo {
                name: a["name"].as_str()?.to_string(),
                pane_id: a["pane_id"].as_str().unwrap_or_default().to_string(),
                status: a["agent_status"].as_str().unwrap_or("unknown").to_string(),
                cwd: a["cwd"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect())
}

/// The checkout path of the focused workspace, from `herdr workspace list`.
/// Plugin actions run from the plugin's own directory, so this — not `$PWD` —
/// is where the human actually is when they open the chat pane.
pub fn parse_focused_cwd(json: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing workspace list JSON")?;
    let workspaces = v["result"]["workspaces"]
        .as_array()
        .context("missing .result.workspaces")?;
    workspaces
        .iter()
        .find(|w| w["focused"].as_bool().unwrap_or(false))
        .and_then(|w| w["worktree"]["checkout_path"].as_str())
        .map(str::to_string)
        .context("no focused workspace with a checkout path")
}

pub fn focused_cwd() -> Result<String> {
    let out = std::process::Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .context("running `herdr workspace list`")?;
    anyhow::ensure!(
        out.status.success(),
        "herdr workspace list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_focused_cwd(&String::from_utf8(out.stdout)?)
}

pub struct RealHerd;

impl HerdControl for RealHerd {
    fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let out = std::process::Command::new("herdr")
            .args(["agent", "list"])
            .output()
            .context("running `herdr agent list`")?;
        anyhow::ensure!(
            out.status.success(),
            "herdr agent list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        parse_agent_list(&String::from_utf8(out.stdout)?)
    }

    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        let out = std::process::Command::new("herdr")
            .args(["agent", "prompt", name, text])
            .output()
            .context("running `herdr agent prompt`")?;
        anyhow::ensure!(
            out.status.success(),
            "herdr agent prompt {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"id":"cli:agent:list","result":{"agents":[
        {"agent":"claude","agent_status":"idle","cwd":"/home/andy/.herdr/worktrees/alare/issue-590","name":"issue-590","pane_id":"w35:p1","tab_id":"w35:t1","workspace_id":"w35"},
        {"agent":"claude","agent_status":"working","name":"issue-758","pane_id":"w3A:p1","tab_id":"w3A:t1","workspace_id":"w3A"},
        {"agent":"claude","agent_status":"idle","pane_id":"w3E:p2","tab_id":"w3E:t2","workspace_id":"w3E"}
    ],"type":"agent_list"}}"#;

    #[test]
    fn parses_named_agents_only() {
        let agents = parse_agent_list(FIXTURE).unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "issue-590");
        assert_eq!(agents[0].status, "idle");
        assert_eq!(agents[0].pane_id, "w35:p1");
        assert_eq!(agents[1].name, "issue-758");
        assert_eq!(agents[1].status, "working");
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_agent_list("not json").is_err());
    }

    #[test]
    fn parses_agent_cwd() {
        let agents = parse_agent_list(FIXTURE).unwrap();
        assert_eq!(agents[0].cwd, "/home/andy/.herdr/worktrees/alare/issue-590");
    }

    #[test]
    fn missing_cwd_is_empty_not_an_error() {
        let agents = parse_agent_list(FIXTURE).unwrap();
        assert_eq!(agents[1].cwd, "");
    }

    const WORKSPACES: &str = r#"{"id":"cli:workspace:list","result":{"type":"workspace_list","workspaces":[
        {"focused":false,"workspace_id":"w2C","worktree":{"checkout_path":"/home/andy/dev/alare-leadership/alare"}},
        {"focused":true,"workspace_id":"w38","worktree":{"checkout_path":"/home/andy/dev/printersrow/kern-app"}}
    ]}}"#;

    #[test]
    fn parses_the_focused_workspace_cwd() {
        assert_eq!(
            parse_focused_cwd(WORKSPACES).unwrap(),
            "/home/andy/dev/printersrow/kern-app"
        );
    }

    #[test]
    fn no_focused_workspace_is_an_error_not_a_silent_first_entry() {
        let json = WORKSPACES.replace("\"focused\":true", "\"focused\":false");
        assert!(parse_focused_cwd(&json).is_err());
    }

    #[test]
    fn focused_workspace_without_a_worktree_is_an_error() {
        let json = r#"{"result":{"workspaces":[{"focused":true,"workspace_id":"w1"}]}}"#;
        assert!(parse_focused_cwd(json).is_err());
    }

    #[test]
    fn rejects_malformed_workspace_json() {
        assert!(parse_focused_cwd("not json").is_err());
    }
}
