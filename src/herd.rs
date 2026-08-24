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

/// Characters of the sent text matched against the composer.
///
/// Deliberately *not* unique to one batch: every batch opens with the same
/// `DELIVERY_RULE`, so this identifies scuttlebutt's outgoing text in
/// general, not this particular send. That is sound only because `herdr
/// agent prompt` replaces the composer contents rather than appending to
/// them — observed on live panes and recorded on #26 — so text matching the
/// fingerprint after a prompt can only be the text that prompt just wrote.
///
/// If herdr ever appends instead, an older batch left unsent on the composer
/// would make a genuinely submitted new batch read as unsubmitted. That
/// costs a repeat delivery and converges on the skip threshold; it does not
/// lose a batch silently, which is the failure this whole path exists to
/// prevent. No unit test can guard a third-party invariant, so it is named
/// here instead.
const FINGERPRINT_CHARS: usize = 40;

/// How long to let the pane repaint before a second look. Paid only when the
/// first look failed to confirm, which on a healthy pane means the prompt
/// landed between the write and the snapshot.
const REREAD_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// Consecutive horizontal box-drawing characters that make a line a rule.
/// Long enough that prose quoting a rule (`"\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}"`) is not
/// mistaken for one.
const RULE_RUN: usize = 8;

/// Whitespace-insensitive form. The composer wraps at word boundaries and
/// pads with NBSP, so comparing normalized text matches across both.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Least composer text that can be taken for a run of our own. A rule inside
/// a message body splits the composer region, leaving only the text below it
/// to match; this is the floor at which that remainder is ours rather than a
/// short phrase someone happened to type.
const SPLIT_TAIL_CHARS: usize = 16;

/// Whether `region` is the tail of a composer whose top a rule inside the
/// message body cut off.
fn is_split_tail(region: &str, sent: &str) -> bool {
    region.chars().count() >= SPLIT_TAIL_CHARS && sent.contains(region)
}

/// A horizontal rule, which is how the composer box is drawn. Any of the
/// box-drawing horizontals — heavy, double, dashed — counts, since which one
/// a terminal UI picks is its own business; a run of them is required so that
/// text merely containing one is content, not furniture.
fn is_rule(line: &str) -> bool {
    let mut run = 0;
    for c in line.chars() {
        run = match c {
            // light, heavy, dashed and double horizontals; the horizontal
            // bar and extension; the block halves a UI may rule with
            '\u{2500}' | '\u{2501}' | '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}'
            | '\u{254c}' | '\u{254d}' | '\u{2550}' | '\u{2015}' | '\u{23af}' | '\u{2580}'
            | '\u{2581}' | '\u{2584}' | '\u{2594}' => run + 1,
            _ => 0,
        };
        if run >= RULE_RUN {
            return true;
        }
    }
    false
}

/// Whether `sent` is sitting unsubmitted on `pane`'s composer — the region
/// between the last two horizontal rules, below which there is only the
/// status footer. It counts as ours when it opens with our fingerprint, or
/// when a rule inside a message body cut the top off and only a run of our
/// own text is left (`is_split_tail`).
///
/// `None` means no composer could be located. That is ambiguous rather than
/// informative: it happens when an unsubmitted batch is tall enough to push
/// its own top rule off the screen, when a pane draws no composer at all,
/// and when a rule is drawn some way this does not recognize. The ambiguity
/// is resolved toward "not submitted" by the caller, because a wrong
/// `Submitted` drops the batch permanently while a wrong `Unconfirmed` costs
/// a repeat delivery and converges on the skip threshold.
fn composer_holds(pane: &str, sent: &str) -> Option<bool> {
    let lines: Vec<&str> = pane.lines().collect();
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_rule(l))
        .map(|(i, _)| i)
        .collect();
    let [.., top, bottom] = rules[..] else {
        return None;
    };
    let region = normalize(&lines[top + 1..bottom].join(" "));
    let sent = normalize(sent);
    let fingerprint: String = sent.chars().take(FINGERPRINT_CHARS).collect();
    Some(region.contains(&fingerprint) || is_split_tail(&region, &sent))
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

/// Reads a pane back to decide whether a prompt was submitted. The pane read
/// and the retry delay are fields so the retry, the delay and the error
/// mapping can be driven without a live herdr.
struct Confirmer {
    read: fn(&str) -> Result<String>,
    delay: std::time::Duration,
}

impl Confirmer {
    fn new() -> Self {
        Confirmer {
            read: read_pane,
            delay: REREAD_DELAY,
        }
    }

    #[cfg(test)]
    fn with_read(read: fn(&str) -> Result<String>) -> Self {
        Confirmer {
            read,
            delay: std::time::Duration::ZERO,
        }
    }

    fn confirm(&self, name: &str, sent: &str) -> Delivery {
        match self.look(name, sent) {
            // The happy path costs one read and no waiting.
            Delivery::Submitted => Delivery::Submitted,
            // A pane that has not repainted yet still shows the text it is
            // about to submit, so give it a moment and look once more before
            // calling a delivery undelivered.
            Delivery::Unconfirmed(_) => {
                std::thread::sleep(self.delay);
                self.look(name, sent)
            }
        }
    }

    fn look(&self, name: &str, sent: &str) -> Delivery {
        match (self.read)(name) {
            Err(e) => Delivery::Unconfirmed(format!("could not read the pane: {e}")),
            Ok(pane) => match composer_holds(&pane, sent) {
                Some(false) => Delivery::Submitted,
                Some(true) => Delivery::Unconfirmed("the text is still on the composer".into()),
                None => Delivery::Unconfirmed("no composer found in the pane".into()),
            },
        }
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
        Ok(Confirmer::new().confirm(name, text))
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

    /// Captured verbatim from `herdr agent read --source visible --format
    /// text` on a live Claude Code pane, transcript above the composer
    /// trimmed. `composer-holds-batch` was taken with a real delivery
    /// preamble typed into the composer and never submitted — the #26 state;
    /// `composer-empty` is the same pane with the composer cleared. Both
    /// keep one transcript line quoting box-drawing characters, which is
    /// what stops `is_rule` from being loosened into matching prose.
    const HOLDS_BATCH: &str = include_str!("../tests/fixtures/composer-holds-batch.txt");
    const EMPTY: &str = include_str!("../tests/fixtures/composer-empty.txt");

    #[test]
    fn a_real_pane_holding_an_unsubmitted_batch_is_not_confirmed() {
        assert_eq!(composer_holds(HOLDS_BATCH, RULE), Some(true));
    }

    #[test]
    fn a_real_pane_with_an_empty_composer_confirms_submission() {
        assert_eq!(composer_holds(EMPTY, RULE), Some(false));
    }

    #[test]
    fn a_real_transcript_line_quoting_a_rule_is_not_a_rule() {
        // Both fixtures carry a line containing `\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}`. Treating it
        // as furniture would move the composer region up into the transcript.
        let quoting: Vec<&str> = HOLDS_BATCH
            .lines()
            .filter(|l| l.contains('\u{2500}') && !is_rule(l))
            .collect();
        assert!(!quoting.is_empty(), "fixture lost its rule-quoting line");
        assert_eq!(composer_holds(HOLDS_BATCH, RULE), Some(true));
    }

    #[test]
    fn heavy_dashed_and_double_rules_are_rules_too() {
        for c in [
            '\u{2500}', '\u{2501}', '\u{2504}', '\u{254c}', '\u{2550}', '\u{2015}', '\u{2581}',
        ] {
            let r = c.to_string().repeat(RULE_RUN);
            assert!(is_rule(&r), "{c:?} run not recognized");
        }
        // a rule with corners, and a titled rule, both still read as rules
        assert!(is_rule(&format!(
            "\u{250c}{}\u{2510}",
            "\u{2500}".repeat(20)
        )));
        assert!(is_rule(&format!(
            "{} Context {}",
            "\u{2500}".repeat(10),
            "\u{2500}".repeat(10)
        )));
        // and a short run in prose is not
        assert!(!is_rule(
            "like \"\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}\", dashed variants"
        ));
    }

    #[test]
    fn a_rule_inside_a_message_body_does_not_hide_the_batch() {
        // A room message can contain a rule of its own. It splits the
        // composer region, leaving only the text after it — which the tail
        // fingerprint still matches.
        let sent = format!("{RULE}\n[scuttlebutt] New messages in the room:\n[#1] a: {}\nthe last line of the batch", "\u{2500}".repeat(30));
        let composer = [
            "\u{276f} Reply only if you have information others don't",
            "  [scuttlebutt] New messages in the room:",
            "  [#1] a:",
            &"\u{2500}".repeat(30),
            "  the last line of the batch",
        ];
        assert_eq!(composer_holds(&pane(&composer), &sent), Some(true));
    }

    fn ok_pane(_: &str) -> Result<String> {
        Ok(EMPTY.to_string())
    }

    fn unreadable(_: &str) -> Result<String> {
        anyhow::bail!("agent target gone not found")
    }

    #[test]
    fn a_confirmed_first_look_costs_no_retry() {
        assert_eq!(
            Confirmer::with_read(ok_pane).confirm("reviewer", RULE),
            Delivery::Submitted
        );
    }

    #[test]
    fn a_pane_that_repaints_between_looks_confirms_on_the_second() {
        // The first snapshot catches the text still on the composer because
        // the pane has not repainted yet. Retrying is what keeps a healthy
        // delivery from being reported as undelivered.
        fn repainting(_: &str) -> Result<String> {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static LOOKS: AtomicUsize = AtomicUsize::new(0);
            Ok(match LOOKS.fetch_add(1, Ordering::SeqCst) {
                0 => HOLDS_BATCH.to_string(),
                _ => EMPTY.to_string(),
            })
        }
        assert_eq!(
            Confirmer::with_read(repainting).confirm("reviewer", RULE),
            Delivery::Submitted
        );
    }

    #[test]
    fn text_still_on_the_composer_after_both_looks_is_unconfirmed() {
        fn stuck(_: &str) -> Result<String> {
            Ok(HOLDS_BATCH.to_string())
        }
        assert_eq!(
            Confirmer::with_read(stuck).confirm("reviewer", RULE),
            Delivery::Unconfirmed("the text is still on the composer".into())
        );
    }

    #[test]
    fn an_unreadable_pane_is_unconfirmed_and_names_the_cause() {
        // The agent's pane closed between the prompt and the read: nothing
        // was delivered, and the operator needs to see why.
        let Delivery::Unconfirmed(why) = Confirmer::with_read(unreadable).confirm("gone", RULE)
        else {
            panic!("an unreadable pane confirmed a delivery");
        };
        assert!(why.contains("could not read the pane"), "why was: {why}");
        assert!(why.contains("not found"), "why was: {why}");
    }

    #[test]
    fn a_pane_with_no_composer_is_unconfirmed() {
        fn no_composer(_: &str) -> Result<String> {
            Ok("a plain shell with no composer at all\n$ ".to_string())
        }
        assert_eq!(
            Confirmer::with_read(no_composer).confirm("reviewer", RULE),
            Delivery::Unconfirmed("no composer found in the pane".into())
        );
    }
}
