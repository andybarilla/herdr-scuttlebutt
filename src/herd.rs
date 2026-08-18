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
}
