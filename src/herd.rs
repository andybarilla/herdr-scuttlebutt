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

/// Prompt markers a composer opens with. Identification is by this set
/// rather than "any punctuation": a marker we do not know is a composer we
/// have not identified, which resolves to `Unconfirmed` and costs a repeat.
/// Guessing instead resolves to `Submitted` and costs the batch.
const MARKERS: [&str; 3] = ["\u{276f}", ">", "\u{203a}"];

/// Words of the composer that must reappear, in order, in what we sent
/// before the composer counts as holding our text. Three rather than a
/// character-count fingerprint because the composer clips, wraps mid-token
/// and pads with NBSP — all of which survive a short run of whole words.
const OVERLAP_WORDS: usize = 3;

/// How long to let the pane repaint before a second look. Paid only when the
/// first look failed to confirm, which on a healthy pane means the prompt
/// landed between the write and the snapshot.
const REREAD_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// Horizontal box-drawing characters a line needs before it counts as a
/// rule, so that a lone `\u{2502}` or a two-character fragment is not one.
const RULE_RUN: usize = 8;

/// Whitespace-insensitive form. The composer wraps at word boundaries and
/// pads with NBSP, so comparing normalized text matches across both.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a line *is* a horizontal rule, which is how a composer box is
/// drawn. Any of the box-drawing horizontals counts — heavy, double, dashed,
/// block — since which one a terminal UI picks is its own business, and
/// corners (square or rounded), tees and verticals may bracket the run.
///
/// The whole line has to be rule characters. A line that merely contains a
/// run is content: a message body carrying box-drawn terminal output reaches
/// the composer with its line breaks intact, and treating one of its lines as
/// furniture would move a region boundary onto it.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    let mut horizontals = 0;
    for c in line.chars() {
        match c {
            // light, heavy, dashed and double horizontals; the horizontal
            // bar and extension; the block halves a UI may rule with
            '\u{2500}' | '\u{2501}' | '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}'
            | '\u{254c}' | '\u{254d}' | '\u{2550}' | '\u{2015}' | '\u{23af}' | '\u{2580}'
            | '\u{2581}' | '\u{2584}' | '\u{2594}' => horizontals += 1,
            // corners, tees, verticals and half-lines may bracket or join it
            '\u{2502}'
            | '\u{2503}'
            | '\u{2506}'
            | '\u{2507}'
            | '\u{250a}'
            | '\u{250b}'
            | '\u{250c}'..='\u{254b}'
            | '\u{254e}'
            | '\u{254f}'
            | '\u{2551}'..='\u{257f}' => {}
            _ => return false,
        }
    }
    horizontals >= RULE_RUN
}

/// The content of every rule-bounded region in `pane` that opens with a
/// prompt marker, marker stripped and whitespace normalized.
///
/// Every such region is returned, not just the last one, and that is the
/// point. A message body ending in a rule of its own, or a bordered box
/// drawn below the composer, both put a *different* region last; picking one
/// region by position is a guess, and a wrong guess here reports a batch
/// submitted that is sitting on a composer two lines up. A transcript echo
/// of a submitted prompt cannot be mistaken for one of these, because the
/// transcript draws no rules around it.
fn composer_regions(pane: &str) -> Vec<String> {
    let lines: Vec<&str> = pane.lines().collect();
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_rule(l))
        .map(|(i, _)| i)
        .collect();
    rules
        .windows(2)
        .filter_map(|w| {
            let region = normalize(&lines[w[0] + 1..w[1]].join(" "));
            let marker = MARKERS.iter().find(|m| region.starts_with(**m))?;
            Some(region[marker.len()..].trim().to_string())
        })
        .collect()
}

/// Whether `content` is a run of `sent` rather than something else on the
/// composer. Matched as whole words in order, so a composer that wrapped the
/// text mid-token or padded it with NBSP still matches: a mangled word kills
/// only the windows it appears in, and any longer run has clean ones.
///
/// The clip marker is cut first, because it fuses onto the last word and
/// there is no window after it to fall back on — `\u{276f} Reply only if\u{2026}` is
/// three words, all of them ours, and none of them matching with the
/// ellipsis still attached. Clipped shorter than that, the content is too
/// short to classify and never reaches a confirmation at all.
fn is_our_text(content: &str, sent: &str) -> bool {
    let content = content
        .trim_end_matches(['\u{2026}', '.', ' '])
        .trim_start_matches(['\u{2026}', '.', ' ']);
    let words: Vec<&str> = content.split_whitespace().collect();
    words
        .windows(OVERLAP_WORDS)
        .any(|w| sent.contains(&w.join(" ")))
}

/// Whether `sent` is sitting unsubmitted on `pane`'s composer.
///
/// `Some(false)` — submitted — is the only answer that advances a cursor and
/// so the only one that can lose a batch, and it is reachable on exactly one
/// path: at least one composer was identified, and every identified composer
/// is either empty or holds text long enough to be recognized as not ours.
/// Everything else is `None`, including cases that look like nothing at all:
/// no rules, no marker, a marker we do not know, or content too short to
/// classify either way.
///
/// The caller resolves `None` toward "not submitted". A wrong `Submitted`
/// drops the batch permanently; a wrong `Unconfirmed` costs a repeat
/// delivery, bounded by the unconfirmed streak. This shape is deliberate:
/// three review rounds found layouts nobody anticipated, and each one was a
/// case that failed to match and fell through to `Submitted`. There is no
/// fall-through here — a layout this does not understand cannot reach it.
fn composer_holds(pane: &str, sent: &str) -> Option<bool> {
    let regions = composer_regions(pane);
    if regions.is_empty() {
        return None;
    }
    let sent = normalize(sent);
    if regions.iter().any(|c| is_our_text(c, &sent)) {
        return Some(true);
    }
    // Non-empty but too short to tell ours from a placeholder or a menu.
    // Idle composers really do carry text that is neither: `\u{276f} Press up to
    // edit queued messages` is what Claude Code shows over a queue.
    let classifiable = |c: &String| {
        let words = c
            .trim_end_matches(['\u{2026}', '.', ' '])
            .split_whitespace()
            .count();
        words == 0 || words >= OVERLAP_WORDS
    };
    match regions.iter().all(classifiable) {
        true => Some(false),
        false => None,
    }
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
                None => Delivery::Unconfirmed("no composer could be identified in the pane".into()),
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

    /// Captured verbatim from `herdr agent read --source visible --format
    /// text` on live Claude Code panes, transcript above the composer
    /// trimmed. `composer-holds-batch` was taken with a real delivery
    /// preamble typed into the composer and never submitted — the #26 state;
    /// `composer-empty` is the same pane cleared; `composer-placeholder` is
    /// a different pane showing the hint Claude Code puts on an idle
    /// composer over a queue, which is text that is neither ours nor a
    /// human's. `composer-holds-batch` and `composer-empty` keep one
    /// transcript line quoting box-drawing characters.
    const HOLDS_BATCH: &str = include_str!("../tests/fixtures/composer-holds-batch.txt");
    const EMPTY: &str = include_str!("../tests/fixtures/composer-empty.txt");
    const PLACEHOLDER: &str = include_str!("../tests/fixtures/composer-placeholder.txt");

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

    /// The batch as it renders on a composer: wrapped, NBSP-padded, and
    /// followed by whatever the caller wants after it.
    fn held(extra: &[&str]) -> Vec<String> {
        let mut lines = vec![
            "\u{276f}\u{a0}Reply only if you have information others don't \u{2014} don't"
                .to_string(),
            "  acknowledge or repeat. Under 80 words; longer belongs on the issue.".to_string(),
            "  [scuttlebutt] New messages in the room:".to_string(),
        ];
        lines.extend(extra.iter().map(|l| l.to_string()));
        lines
    }

    fn holds(composer: &[String], sent: &str) -> Option<bool> {
        let refs: Vec<&str> = composer.iter().map(String::as_str).collect();
        composer_holds(&pane(&refs), sent)
    }

    // ---- the real panes -------------------------------------------------

    #[test]
    fn a_real_pane_holding_an_unsubmitted_batch_is_not_confirmed() {
        assert_eq!(composer_holds(HOLDS_BATCH, RULE), Some(true));
    }

    #[test]
    fn a_real_pane_with_an_empty_composer_confirms_submission() {
        assert_eq!(composer_holds(EMPTY, RULE), Some(false));
    }

    #[test]
    fn a_real_placeholder_composer_confirms_submission() {
        // `\u{276f} Press up to edit queued messages` is the hint Claude Code shows
        // over a queue. Requiring an empty composer would leave every agent
        // in that state permanently unconfirmable, and the streak would then
        // skip real batches.
        assert_eq!(composer_holds(PLACEHOLDER, RULE), Some(false));
    }

    #[test]
    fn a_real_transcript_line_quoting_a_rule_is_not_a_rule() {
        let quoting: Vec<&str> = HOLDS_BATCH
            .lines()
            .filter(|l| l.contains('\u{2500}') && !is_rule(l))
            .collect();
        assert!(!quoting.is_empty(), "fixture lost its rule-quoting line");
        assert_eq!(composer_holds(HOLDS_BATCH, RULE), Some(true));
    }

    // ---- nothing identified is never a confirmation ---------------------

    #[test]
    fn a_pane_with_no_rules_identifies_no_composer() {
        assert_eq!(composer_holds("a plain shell\n$ ", RULE), None);
        assert_eq!(composer_holds("", RULE), None);
    }

    #[test]
    fn a_region_without_a_known_marker_identifies_no_composer() {
        // A bordered box that is not a composer — a notification band, a
        // permission prompt — must not be read as one and reported clear.
        let composer = ["  Allow this command? [y/N]"];
        assert_eq!(holds(&composer.map(String::from), RULE), None);
    }

    #[test]
    fn an_unrecognized_marker_identifies_no_composer() {
        let composer = ["\u{2794} Reply only if you have information others don't"];
        assert_eq!(holds(&composer.map(String::from), RULE), None);
    }

    #[test]
    fn content_too_short_to_classify_is_not_a_confirmation() {
        // One or two words could be a placeholder, a menu row, or the last
        // clipped fragment of our own batch. Unclassifiable is not clear.
        for short in ["-", "\u{1f389}", "ok", "5"] {
            let composer = [format!("\u{276f} {short}")];
            assert_eq!(holds(&composer, RULE), None, "{short:?} was classified");
        }
    }

    // ---- the boundary cannot be relocated -------------------------------

    #[test]
    fn a_run_inside_a_message_body_does_not_relocate_the_boundary() {
        // Message bodies keep their line breaks through `scrub`, so a pasted
        // line of block-drawn output lands in the composer as its own line.
        let composer = held(&["  [#1] someone: load \u{2581}\u{2581}\u{2581}\u{2581}\u{2581}\u{2584}\u{2584}\u{2584} ok"]);
        assert_eq!(holds(&composer, RULE), Some(true));
    }

    #[test]
    fn a_batch_ending_in_a_rule_is_still_found() {
        // The message's own rule splits the composer in two. The half that
        // opens with the marker is still a composer and still holds us.
        let composer = held(&[&"\u{2500}".repeat(30)]);
        assert_eq!(holds(&composer, RULE), Some(true));
    }

    #[test]
    fn a_one_character_tail_below_a_body_rule_is_not_a_confirmation() {
        // The tail region has no marker, so it is not a composer at all; the
        // half above it is, and it holds the batch.
        let composer = held(&[&"\u{2500}".repeat(30), "  -"]);
        assert_eq!(holds(&composer, RULE), Some(true));
    }

    #[test]
    fn a_box_below_the_composer_does_not_hide_the_batch() {
        // A permission prompt or notification band drawn under the composer
        // puts a different region last. Every marker-led region is checked,
        // so the batch two lines up is still found.
        let mut composer = held(&[]);
        composer.push("\u{2500}".repeat(60));
        composer.push("\u{276f} 1. Yes, allow this command to run".into());
        assert_eq!(holds(&composer, RULE), Some(true));
    }

    #[test]
    fn a_rounded_composer_box_is_still_a_composer() {
        // Synthetic: no pane here draws one, but a UI that did would
        // otherwise identify nothing on every tick.
        let rounded = format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(40));
        let pane = format!("\u{25cf} done\n{rounded}\n\u{276f}\n{rounded}\n  status line");
        assert_eq!(composer_holds(&pane, RULE), Some(false));
    }

    // ---- matching what is there -----------------------------------------

    #[test]
    fn a_wrapped_and_nbsp_padded_composer_still_matches() {
        assert_eq!(holds(&held(&[]), RULE), Some(true));
    }

    #[test]
    fn a_hard_wrapped_token_still_matches() {
        // A narrow pane breaks mid-token, so normalize sees two words where
        // we sent one. Whole-word runs elsewhere in the batch still match.
        let sent = format!("{RULE} see https://example.com/a/very/long/path for the rest");
        let composer = [
            "\u{276f} Reply only if you have information others don't \u{2014} don't".to_string(),
            "  acknowledge or repeat. see https://example.com/a/very/lo".to_string(),
            "  ng/path for the rest".to_string(),
        ];
        assert_eq!(holds(&composer, &sent), Some(true));
    }

    #[test]
    fn a_clipped_composer_still_matches() {
        // Claude Code clips a tall composer; three words of ours is enough.
        let composer = ["\u{276f} Reply only if\u{2026}".to_string()];
        assert_eq!(holds(&composer, RULE), Some(true));
    }

    #[test]
    fn a_composer_clipped_mid_word_still_matches() {
        // Clipping does not always leave an ellipsis to cut, so the last
        // word can arrive truncated. It matches because a window is compared
        // as a substring rather than as whole tokens, and a truncated final
        // word is a prefix of the real one. Pinned because switching that
        // comparison to word boundaries would reopen a fall-through: three
        // words, all of them ours, matching nothing, reported submitted.
        let composer = ["\u{276f} Reply only i".to_string()];
        assert_eq!(holds(&composer, RULE), Some(true));
    }

    #[test]
    fn someone_elses_text_on_the_composer_confirms_submission() {
        // A human typing at the pane did not stop our delivery landing, and
        // telling their text from a tool's is #24, which stays out of scope.
        let composer = ["\u{276f} stop posting to the room and stand down".to_string()];
        assert_eq!(holds(&composer, RULE), Some(false));
    }

    #[test]
    fn the_same_text_in_the_transcript_confirms_submission() {
        // Submitted text is echoed above the composer, with no rules drawn
        // around it, so it forms no region and cannot be mistaken for one.
        let echoed = format!("\u{276f} {RULE}\n{}", EMPTY);
        assert_eq!(composer_holds(&echoed, RULE), Some(false));
    }

    #[test]
    fn no_layout_change_turns_a_held_batch_into_a_confirmation() {
        // The property, stated directly. Three review rounds each found a
        // layout that made a held batch read as submitted, and each was a
        // different input rather than a different bug. Any perturbation may
        // cost identification — `None`, a repeat delivery — but none of them
        // may reach `Some(false)`, which advances the cursor and drops it.
        let rounded = format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(60));
        let held = held(&[]).join("\n");
        let perturbed = [
            // a bordered box drawn below the composer
            format!(
                "{HOLDS_BATCH}\n{}\n\u{276f} 1. Yes\n{}",
                "\u{2500}".repeat(60),
                "\u{2500}".repeat(60)
            ),
            // the composer's own box drawn with rounded corners
            format!("\u{25cf} done\n{rounded}\n{held}\n{rounded}\n  status"),
            // a rule inside a message body, with tails of several lengths
            format!(
                "{}\n{held}\n{}\n  -\n{}",
                "\u{2500}".repeat(60),
                "\u{2500}".repeat(60),
                "\u{2500}".repeat(60)
            ),
            format!(
                "{}\n{held}\n{}\n  thanks all\n{}",
                "\u{2500}".repeat(60),
                "\u{2500}".repeat(60),
                "\u{2500}".repeat(60)
            ),
            // the top rule scrolled off a narrow screen
            HOLDS_BATCH.lines().skip(5).collect::<Vec<_>>().join("\n"),
            // an unfamiliar marker, and none at all
            HOLDS_BATCH.replace('\u{276f}', "\u{2794}"),
            HOLDS_BATCH.replace('\u{276f}', " "),
            // no furniture whatsoever
            held.clone(),
        ];
        for (i, pane) in perturbed.iter().enumerate() {
            assert_ne!(
                composer_holds(pane, RULE),
                Some(false),
                "perturbation {i} reported a held batch as submitted"
            );
        }
    }

    #[test]
    fn heavy_dashed_and_double_rules_are_rules_too() {
        for c in [
            '\u{2500}', '\u{2501}', '\u{2504}', '\u{254c}', '\u{2550}', '\u{2015}', '\u{2581}',
        ] {
            let r = c.to_string().repeat(RULE_RUN);
            assert!(is_rule(&r), "{c:?} run not recognized");
        }
        assert!(is_rule(&format!(
            "  \u{250c}{}\u{2510}  ",
            "\u{2500}".repeat(20)
        )));
        assert!(is_rule(&format!(
            "\u{256d}{}\u{256e}",
            "\u{2500}".repeat(20)
        )));
        assert!(!is_rule("\u{2500}\u{2500}"));
        assert!(!is_rule(""));
    }

    #[test]
    fn a_line_merely_containing_a_run_is_content_not_furniture() {
        assert!(!is_rule(
            "like \"\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}\", dashed variants"
        ));
        assert!(!is_rule("[#1] someone: load \u{2581}\u{2581}\u{2581}\u{2581}\u{2581}\u{2584}\u{2584}\u{2584} ok"));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_confirmed_first_look_costs_one_read_and_no_retry() {
        // The retry costs a `REREAD_DELAY` on every delivery if it is not
        // skipped on the happy path, so the read count is the assertion —
        // `Submitted` alone would hold however many looks it took.
        // The counter lives in the test body, so no other test can reach
        // it. A module-scope static reset by its one owner is correct only
        // by convention, and #30 is that convention decaying.
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn cleared(_: &str) -> Result<String> {
            READS.fetch_add(1, Ordering::SeqCst);
            Ok(EMPTY.to_string())
        }
        assert_eq!(
            Confirmer::with_read(cleared).confirm("reviewer", RULE),
            Delivery::Submitted
        );
        assert_eq!(READS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_pane_that_repaints_between_looks_confirms_on_the_second() {
        // The first snapshot catches the text still on the composer because
        // the pane has not repainted yet. Retrying is what keeps a healthy
        // delivery from being reported as undelivered.
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn repainting(_: &str) -> Result<String> {
            Ok(match READS.fetch_add(1, Ordering::SeqCst) {
                0 => HOLDS_BATCH.to_string(),
                _ => EMPTY.to_string(),
            })
        }
        assert_eq!(
            Confirmer::with_read(repainting).confirm("reviewer", RULE),
            Delivery::Submitted
        );
        assert_eq!(READS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn text_still_on_the_composer_after_both_looks_is_unconfirmed() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn stuck(_: &str) -> Result<String> {
            READS.fetch_add(1, Ordering::SeqCst);
            Ok(HOLDS_BATCH.to_string())
        }
        assert_eq!(
            Confirmer::with_read(stuck).confirm("reviewer", RULE),
            Delivery::Unconfirmed("the text is still on the composer".into())
        );
        // two looks and no more: the wait is paid once, not per tick
        assert_eq!(READS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_unreadable_pane_is_unconfirmed_and_names_the_cause() {
        // The agent's pane closed between the prompt and the read: nothing
        // was delivered, and the operator needs to see why.
        fn unreadable(_: &str) -> Result<String> {
            anyhow::bail!("agent target gone not found")
        }
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
            Delivery::Unconfirmed("no composer could be identified in the pane".into())
        );
    }
}
