use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub name: String,
    pub pane_id: String,
    pub status: String,
    pub cwd: String,
    /// Whether the human's cursor is in this agent's pane. `None` means
    /// `herdr agent list` did not emit the field at all, which is treated as
    /// "not focused" at the delivery gate — see `focus_blocked` in `daemon.rs`.
    pub focused: Option<bool>,
}

/// The outcome of a prompt herdr accepted. `herdr agent prompt` can return
/// success with the text typed into the agent's composer and never submitted
/// (herdrdev/herdr#2422), so its `Ok` means *accepted*, not *delivered*.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// Positive evidence the text left the composer.
    Submitted,
    /// Accepted, with no such evidence. `why` names what was observed.
    Unconfirmed(String),
}

pub trait HerdControl {
    fn list_agents(&self) -> Result<Vec<AgentInfo>>;
    fn prompt(&self, name: &str, text: &str) -> Result<Delivery>;
}

/// Characters of the sent text matched against the composer. Long enough to
/// be unique to scuttlebutt's own outgoing text, short enough to survive a
/// composer that clips the line it is rendered on.
const FINGERPRINT_CHARS: usize = 40;

/// How long to let the pane repaint before a second look. Paid only when the
/// first read found the text still on the composer, which on a healthy pane
/// means the prompt landed between the write and the snapshot.
const REREAD_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// Whitespace-insensitive form. The composer wraps at word boundaries and
/// pads with NBSP, so comparing normalized text matches across both.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fingerprint(sent: &str) -> String {
    normalize(sent).chars().take(FINGERPRINT_CHARS).collect()
}

/// A horizontal rule, which is how the composer box is drawn. Box-drawing
/// characters only: message bodies reach the pane unaltered, so a line of
/// ASCII `---` in someone's post is content, not furniture.
fn is_border(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| matches!(c, '\u{2500}' | '\u{2501}' | '\u{2550}'))
}

/// Whether `sent` is sitting unsubmitted on `pane`'s composer — the region
/// between the last two horizontal rules, below which there is only the
/// status footer.
///
/// `None` means the composer could not be located, which is itself evidence
/// against submission: a submitted prompt leaves an empty one-line box with
/// both rules on screen, while an unsubmitted batch is tall enough to push
/// its own top rule off it.
fn composer_holds(pane: &str, sent: &str) -> Option<bool> {
    let lines: Vec<&str> = pane.lines().collect();
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_border(l))
        .map(|(i, _)| i)
        .collect();
    let [.., top, bottom] = rules[..] else {
        return None;
    };
    let region = normalize(&lines[top + 1..bottom].join(" "));
    Some(region.contains(&fingerprint(sent)))
}

fn read_pane(name: &str) -> Result<String> {
    let out = std::process::Command::new("herdr")
        .args([
            "agent", "read", name, "--source", "visible", "--format", "text",
        ])
        .output()
        .context("running `herdr agent read`")?;
    // The socket API reports its errors on stdout (`agent_not_found` for a
    // pane that has since closed), so stderr alone would log an empty cause.
    anyhow::ensure!(
        out.status.success(),
        "herdr agent read {name} failed: {}{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?)
}

/// Reads `name`'s pane back and reports whether `sent` actually left the
/// composer.
fn confirm(name: &str, sent: &str) -> Delivery {
    match look(name, sent) {
        // The happy path costs one read and no waiting.
        Delivery::Submitted => Delivery::Submitted,
        // A pane that has not repainted yet still shows the text it is about
        // to submit, so give it a moment and look once more before calling a
        // delivery undelivered.
        Delivery::Unconfirmed(_) => {
            std::thread::sleep(REREAD_DELAY);
            look(name, sent)
        }
    }
}

fn look(name: &str, sent: &str) -> Delivery {
    match read_pane(name) {
        Err(e) => Delivery::Unconfirmed(format!("could not read the pane: {e}")),
        Ok(pane) => match composer_holds(&pane, sent) {
            Some(false) => Delivery::Submitted,
            Some(true) => Delivery::Unconfirmed("the text is still on the composer".into()),
            None => Delivery::Unconfirmed("no composer found in the pane".into()),
        },
    }
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
                focused: a["focused"].as_bool(),
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

    fn prompt(&self, name: &str, text: &str) -> Result<Delivery> {
        let out = std::process::Command::new("herdr")
            .args(["agent", "prompt", name, text])
            .output()
            .context("running `herdr agent prompt`")?;
        anyhow::ensure!(
            out.status.success(),
            "herdr agent prompt {name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(confirm(name, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"id":"cli:agent:list","result":{"agents":[
        {"agent":"claude","agent_status":"idle","cwd":"/home/andy/.herdr/worktrees/alare/issue-590","focused":true,"name":"issue-590","pane_id":"w35:p1","tab_id":"w35:t1","workspace_id":"w35"},
        {"agent":"claude","agent_status":"working","focused":false,"name":"issue-758","pane_id":"w3A:p1","tab_id":"w3A:t1","workspace_id":"w3A"},
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
    fn parses_the_focused_flag_both_ways() {
        let agents = parse_agent_list(FIXTURE).unwrap();
        assert_eq!(agents[0].focused, Some(true));
        assert_eq!(agents[1].focused, Some(false));
    }

    #[test]
    fn missing_focused_is_none_not_false() {
        // A herdr that does not emit the field must be distinguishable from
        // one reporting an unfocused pane: the delivery gate logs the former
        // once and then delivers anyway.
        let json = r#"{"result":{"agents":[
            {"agent_status":"idle","name":"issue-590","pane_id":"w35:p1"}
        ]}}"#;
        let agents = parse_agent_list(json).unwrap();
        assert_eq!(agents[0].focused, None);
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

    const RULE: &str =
        "Reply only if you have information others don't \u{2014} don't acknowledge or repeat.";

    /// A pane rendered the way herdr's `visible` snapshot returns it: a
    /// transcript, the composer box, then the status footer.
    fn pane(composer: &[&str]) -> String {
        let rule = "\u{2500}".repeat(60);
        let mut out = vec![
            "\u{25cf} Nothing to add.".to_string(),
            String::new(),
            "\u{273b} Worked for 1s".to_string(),
            String::new(),
            rule.clone(),
        ];
        out.extend(composer.iter().map(|l| l.to_string()));
        out.push(rule);
        out.push("  andy@apbfw16 ~/dev/alare main  [Opus 5] ctx:24%".into());
        out.push("  \u{23f5}\u{23f5} auto mode on (shift+tab to cycle)".into());
        out.join("\n")
    }

    #[test]
    fn an_empty_composer_confirms_submission() {
        assert_eq!(composer_holds(&pane(&["\u{276f}"]), RULE), Some(false));
    }

    #[test]
    fn our_own_text_on_the_composer_is_not_submitted() {
        // The #26 pane: herdr returned success, the batch is sitting unsent.
        let composer = [
            "\u{276f} Reply only if you have information others don't \u{2014} don't",
            "  acknowledge or repeat. Under 80 words; longer belongs on the issue.",
            "  [scuttlebutt] New messages in the room:",
        ];
        assert_eq!(composer_holds(&pane(&composer), RULE), Some(true));
    }

    #[test]
    fn someone_elses_text_on_the_composer_confirms_submission() {
        // A human typing at the pane is not our text: the delivery still
        // went through, and #24 stays out of scope.
        let composer = ["\u{276f} stop posting to the room and stand down"];
        assert_eq!(composer_holds(&pane(&composer), RULE), Some(false));
    }

    #[test]
    fn the_same_text_in_the_transcript_confirms_submission() {
        // Submitted text is echoed above the composer. Only the composer
        // region is consulted, or every delivery would look undelivered.
        let echoed = format!("\u{276f} {RULE}\n{}", pane(&["\u{276f}"]));
        assert_eq!(composer_holds(&echoed, RULE), Some(false));
    }

    #[test]
    fn a_wrapped_composer_still_matches() {
        // The composer wraps at word boundaries and pads with NBSP; matching
        // has to see through both.
        let composer = [
            "\u{276f}\u{a0}Reply  only if you",
            "  have information others don't \u{2014} don't acknowledge or repeat.",
        ];
        assert_eq!(composer_holds(&pane(&composer), RULE), Some(true));
    }

    #[test]
    fn a_composer_that_cannot_be_located_is_not_confirmed() {
        // A batch tall enough to push its own top rule off screen. A
        // submitted prompt always leaves both rules on screen, so an
        // unlocatable composer is evidence against submission, not for it.
        let clipped = format!(
            "  {RULE}\n  [scuttlebutt] New messages in the room:\n{}\n  status",
            "\u{2500}".repeat(60)
        );
        assert_eq!(composer_holds(&clipped, RULE), None);
        assert_eq!(composer_holds("", RULE), None);
    }

    #[test]
    fn ascii_rules_in_a_message_are_content_not_furniture() {
        // A post containing `-----` reaches the pane unaltered; reading it as
        // a composer edge would move the region and hide the real text.
        let composer = [
            "\u{276f} Reply only if you have information others don't \u{2014} don't",
            "  -------------------------",
            "  acknowledge or repeat.",
        ];
        assert_eq!(composer_holds(&pane(&composer), RULE), Some(true));
    }
}
