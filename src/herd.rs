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

/// Prompt markers a rule-bounded composer opens with. Identification is by
/// this set rather than "any punctuation": a marker we do not know is a
/// composer we have not identified, which resolves to `Unconfirmed` and
/// costs a repeat. Guessing instead resolves to `Submitted` and costs the
/// batch. A gutter-bounded composer has no marker and is identified by
/// `gutter_bounded` instead.
const MARKERS: [&str; 3] = ["\u{276f}", ">", "\u{203a}"];

/// Verticals an editor may draw down the left edge of its input box on
/// every row, in place of a rule above it. OpenCode uses the heavy one.
const GUTTERS: [char; 2] = ['\u{2503}', '\u{2502}'];

/// Fragments of what a composer shows in place of its contents while a
/// queue is holding messages — Claude Code shows `\u{276f} Press up to edit queued
/// messages`. Such a hint is not ours and not a human's, and — this is the
/// point — it hides the queue rather than describing it, so it cannot tell
/// us whether our batch reached that queue or never left `herdr agent
/// prompt`. Recognized so a word count cannot read it as a cleared
/// composer.
///
/// A fragment rather than the whole line, because the whole line is the
/// part that moves: a count, a plural, or an appended key hint would stop
/// an exact match firing, and the pane would fall back to `Some(false)` —
/// silently, since nothing in the delivery path can tell a hint it failed
/// to recognize from a composer that is genuinely clear.
const QUEUE_HINTS: [&str; 1] = ["queued messages"];

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

/// The horizontals a rule is drawn from: light, heavy, dashed and double;
/// the horizontal bar and extension; the block halves a UI may rule with.
/// Which one a terminal UI picks is its own business.
fn is_horizontal(c: char) -> bool {
    matches!(
        c,
        '\u{2500}'
            | '\u{2501}'
            | '\u{2504}'
            | '\u{2505}'
            | '\u{2508}'
            | '\u{2509}'
            | '\u{254c}'
            | '\u{254d}'
            | '\u{2550}'
            | '\u{2015}'
            | '\u{23af}'
            | '\u{2580}'
            | '\u{2581}'
            | '\u{2584}'
            | '\u{2594}'
    )
}

/// Box-drawing furniture that may bracket or join a run: corners (square or
/// rounded), tees, verticals and half-lines.
fn is_joint(c: char) -> bool {
    matches!(
        c,
        '\u{2502}'
            | '\u{2503}'
            | '\u{2506}'
            | '\u{2507}'
            | '\u{250a}'
            | '\u{250b}'
            | '\u{250c}'..='\u{254b}'
            | '\u{254e}'
            | '\u{254f}'
            | '\u{2551}'..='\u{257f}'
    )
}

/// Whether a line *is* a horizontal rule, which is how a composer box is
/// drawn.
///
/// The line has to open and close with box-drawing characters and carry a
/// run of horizontals. Between them it may carry one label: Claude Code
/// centres the session title in the top border of its composer
/// (`\u{2500}\u{2500} clear-conversation-state \u{2500}\u{2500}`), which is #36 — requiring every
/// character to be box-drawing rejected that border, left one rule in the
/// pane, and identified no composer at all.
///
/// One label, not any number of them, and never at either end. That is what
/// still separates furniture from content: a message body carrying
/// box-drawn terminal output reaches the composer with its line breaks
/// intact, and treating one of its lines as furniture would move a region
/// boundary onto it. Such a line all but always has text outside the run —
/// `like "\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}", dashed variants` opens with a word.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    let boxed = |c: char| is_horizontal(c) || is_joint(c);
    if !line.starts_with(boxed) || !line.ends_with(boxed) {
        return false;
    }
    let mut horizontals = 0;
    let mut labels = 0;
    let mut in_label = false;
    for c in line.chars() {
        if boxed(c) {
            in_label = false;
            horizontals += usize::from(is_horizontal(c));
        } else if !in_label {
            in_label = true;
            labels += 1;
        }
    }
    horizontals >= RULE_RUN && labels <= 1
}

/// Every composer identified in `pane`, each as its own lines, normalized
/// and stripped of the marker or gutter that identified it.
///
/// Lines rather than one joined string because the two decisions want
/// different views of a composer. Whether it holds our text reads best
/// across the whole thing, since the composer wraps mid-sentence. Whether
/// it is *clear* has to read line by line: a box that prints furniture of
/// its own below the text — OpenCode's model footer — pads a two-word
/// unsubmitted prompt past the word count that would otherwise call it too
/// short to classify.
///
/// The two layouts here are the two this fleet runs, and neither is a
/// guess: a rule-bounded box that opens with a known prompt marker, and a
/// gutter-bounded box at the bottom edge of the pane. A layout matching
/// neither identifies nothing, which costs a repeat delivery.
fn composer_regions(pane: &str) -> Vec<Vec<String>> {
    let lines: Vec<&str> = pane.lines().collect();
    let mut regions = rule_bounded(&lines);
    regions.extend(gutter_bounded(&lines));
    regions
}

/// Every rule-bounded region that opens with a prompt marker.
///
/// Every such region is returned, not just the last one, and that is the
/// point. A message body ending in a rule of its own, or a bordered box
/// drawn below the composer, both put a *different* region last; picking one
/// region by position is a guess, and a wrong guess here reports a batch
/// submitted that is sitting on a composer two lines up. A transcript echo
/// of a submitted prompt cannot be mistaken for one of these, because the
/// transcript draws no rules around it.
fn rule_bounded(lines: &[&str]) -> Vec<Vec<String>> {
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_rule(l))
        .map(|(i, _)| i)
        .collect();
    rules
        .windows(2)
        .filter_map(|w| {
            let mut region: Vec<String> = lines[w[0] + 1..w[1]]
                .iter()
                .map(|l| normalize(l))
                .filter(|l| !l.is_empty())
                .collect();
            let first = region.first()?;
            let marker = MARKERS.iter().find(|m| first.starts_with(**m))?.len();
            region[0] = region[0][marker..].trim().to_string();
            Some(region)
        })
        .collect()
}

/// The columns a box occupies, read off the bottom edge that closes it.
///
/// A pane right-aligns furniture of its own outside that edge — OpenCode
/// wraps a long working directory up the right margin, across the rows the
/// composer is drawn on — and clipping to the box is what keeps that out of
/// the composer's contents. Left in, it reads as text that is not ours: a
/// two-word unsubmitted prompt beside it is no longer too short to
/// classify, which is `Some(false)` and the batch gone.
///
/// Columns are counted in characters. A row carrying a double-width
/// character is clipped a little late and keeps a fragment of what sits
/// beyond the edge, which can only add words to a row that already had our
/// text on it. It cannot empty a row: nothing the composer draws lies
/// outside its own box.
fn box_span(border: &str) -> (usize, usize) {
    let start = border.chars().take_while(|c| c.is_whitespace()).count();
    (start, border.trim_end().chars().count())
}

/// One row of a gutter-bounded box, clipped to the box, gutter stripped and
/// normalized.
fn boxed_row(row: &str, (start, end): (usize, usize)) -> String {
    let clipped: String = row.chars().take(end).skip(start).collect();
    normalize(clipped.trim_start_matches(|c| GUTTERS.contains(&c)))
}

/// The composer of an editor that draws a vertical down every row of its
/// input box instead of a rule above it. OpenCode draws one, and puts a
/// single horizontal in the whole pane: the bottom edge of that box. One
/// rule bounds no region, which is the other half of #36 — and no widening
/// of `is_rule` can reach it, because the rules it would need are not
/// drawn.
///
/// Identification is positional, and it has to be: this layout gives the
/// composer no marker of its own, and it draws *submitted* messages in the
/// transcript inside the same gutter. Sitting against the pane's bottom
/// edge is what separates the composer from those echoes — an echo always
/// has the rest of the transcript below it.
fn gutter_bounded(lines: &[&str]) -> Option<Vec<String>> {
    let gutter = |l: &&str| l.trim_start().starts_with(|c| GUTTERS.contains(&c));
    let bottom = lines.iter().rposition(|l| is_rule(l))?;
    let mut top = bottom;
    while top > 0 && gutter(&lines[top - 1]) {
        top -= 1;
    }
    // A box, not a single bordered line: one gutter line above the bottom
    // edge is as easily a transcript echo that happens to end there.
    if bottom - top < 2 {
        return None;
    }
    let span = box_span(lines[bottom]);
    let mut region: Vec<String> = lines[top..bottom]
        .iter()
        .map(|l| boxed_row(l, span))
        .collect();
    // OpenCode prints the agent and model inside the box, below a blank row
    // and under anything typed. That is furniture: left in, it pads an
    // empty composer to three words, and a short unsubmitted prompt beside
    // it would read as too long to be ours rather than too short to
    // classify — `Some(false)`, and the batch gone. It is dropped only when
    // that exact shape is present, because dropping a line that turned out
    // to be ours is the one error here that could also lose a batch.
    let footer = region.len() >= 2
        && !region[region.len() - 1].is_empty()
        && region[region.len() - 2].is_empty();
    if footer {
        region.pop();
    }
    Some(region)
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
/// no composer identified in either layout, a marker we do not know, a
/// queue hint standing in for the composer's contents, or content too short
/// to classify either way.
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
    if regions.iter().any(|c| is_our_text(&c.join(" "), &sent)) {
        return Some(true);
    }
    let lines = || regions.iter().flatten();
    // A composer showing a queue hint is showing neither our text nor a
    // clear box: the queue it names may hold our batch or may not, and
    // nothing in the pane says which.
    if lines().any(|l| QUEUE_HINTS.iter().any(|h| l.contains(h))) {
        return None;
    }
    // Non-empty but too short to tell ours from a placeholder or a menu.
    // Per line, because a composer that draws furniture of its own below
    // the text would otherwise carry every short line past this on the
    // strength of the furniture's word count.
    let classifiable = |c: &String| {
        let words = c
            .trim_end_matches(['\u{2026}', '.', ' '])
            .split_whitespace()
            .count();
        words == 0 || words >= OVERLAP_WORDS
    };
    match lines().all(classifiable) {
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

    /// A Claude Code pane whose composer border carries the session title,
    /// which is the shape #36 was filed on. Captured from a live lead pane
    /// with a clear composer.
    const TITLED: &str = include_str!("../tests/fixtures/composer-titled-rule.txt");

    /// OpenCode panes, captured the same way. The composer is bounded by a
    /// `\u{2503}` on every row and a single block rule along the bottom, and it
    /// carries the agent and model on its last row.
    ///
    /// `opencode-holds-batch` and `opencode-empty` are one pane before and
    /// after `\u{2503}`-drawn text was typed into it, and both keep the transcript
    /// echo of an identical batch that *was* submitted — the echo is drawn
    /// in the same gutter as the composer, so a locator that finds it
    /// reports a cleared composer as holding us forever.
    /// `opencode-wrapped`, `opencode-short` and `opencode-hint` are a
    /// narrower pane holding a wrapped batch, holding two words, and clear
    /// under OpenCode's own idle hint. `opencode-live-room` is a working
    /// lead pane from another room with three echoes above a clear
    /// composer.
    const OC_HOLDS: &str = include_str!("../tests/fixtures/opencode-holds-batch.txt");
    const OC_EMPTY: &str = include_str!("../tests/fixtures/opencode-empty.txt");
    const OC_WRAPPED: &str = include_str!("../tests/fixtures/opencode-wrapped.txt");
    const OC_SHORT: &str = include_str!("../tests/fixtures/opencode-short.txt");
    const OC_HINT: &str = include_str!("../tests/fixtures/opencode-hint.txt");
    const OC_LIVE: &str = include_str!("../tests/fixtures/opencode-live-room.txt");
    const OC_WRAPPED_CWD: &str = include_str!("../tests/fixtures/opencode-wrapped-cwd.txt");

    /// What was typed into the OpenCode panes: `RULE` as a real delivery
    /// carries it, with the sentence that follows it in the preamble.
    const OC_SENT: &str = "Reply only if you have information others don't \u{2014} don't \
         acknowledge or repeat. Under 80 words; longer belongs on the issue.";

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
    fn a_real_queue_hint_confirms_nothing() {
        // `\u{276f} Press up to edit queued messages` is what Claude Code shows over
        // a queue. This asserted `Some(false)` until #36: five words, none
        // of them ours, read as a cleared composer. But the hint replaces
        // the queue's contents rather than describing them, so it is
        // equally the pane of a batch that never left `herdr agent prompt`
        // — and `Some(false)` there advances the cursor over it.
        //
        // The cost is a repeat delivery per tick while a queue stands, and
        // `MAX_BATCH_FAILURES` still force-advances after five of those
        // (#39). That is a bounded, logged skip; this was a silent one.
        assert_eq!(composer_holds(PLACEHOLDER, RULE), None);
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

    // ---- the layouts this fleet runs ------------------------------------

    #[test]
    fn every_real_pane_identifies_a_composer() {
        // #36 in one assertion: none of these identified anything, so every
        // delivery to them was unconfirmable and the failure threshold
        // skipped the batch after five tries.
        for (name, pane) in [
            ("claude code, titled border", TITLED),
            ("claude code, plain border", EMPTY),
            ("opencode, clear", OC_EMPTY),
            ("opencode, holding a batch", OC_HOLDS),
            ("opencode, a working lead", OC_LIVE),
            ("opencode, a wrapped working directory", OC_WRAPPED_CWD),
        ] {
            assert!(
                !composer_regions(pane).is_empty(),
                "{name}: no composer identified"
            );
        }
    }

    #[test]
    fn a_session_title_in_the_composer_border_is_still_a_rule() {
        let titled = TITLED
            .lines()
            .find(|l| l.contains("clear-conversation-state"))
            .expect("fixture lost its titled border");
        assert!(is_rule(titled), "{titled:?} not recognized");
        assert_eq!(composer_holds(TITLED, RULE), Some(false));
    }

    #[test]
    fn a_titled_border_over_a_held_batch_is_not_a_confirmation() {
        // The two halves put together: the real titled border from one pane
        // above the real held batch of another. Live panes gave each shape
        // separately, and it is their combination that would drop a batch.
        let titled = TITLED
            .lines()
            .find(|l| l.contains("clear-conversation-state"))
            .expect("fixture lost its titled border");
        let mut lines: Vec<&str> = HOLDS_BATCH.lines().collect();
        let top = lines
            .iter()
            .position(|l| is_rule(l))
            .expect("fixture lost its composer border");
        lines[top] = titled;
        assert_eq!(composer_holds(&lines.join("\n"), RULE), Some(true));
    }

    #[test]
    fn a_queue_hint_that_has_moved_still_confirms_nothing() {
        // Synthetic, and that is the point: the hint's wording is the part
        // most likely to change under us, and an exact match failing to
        // fire fails toward `Some(false)`, which is the verdict that loses
        // the batch. Pinned so that shape cannot come back.
        for hint in [
            "Press up to edit 3 queued messages",
            "Press up to edit queued messages (esc to clear)",
        ] {
            let composer = [format!("\u{276f} {hint}")];
            assert_eq!(holds(&composer, RULE), None, "{hint:?} was classified");
        }
    }

    #[test]
    fn a_gutter_drawn_composer_holding_a_batch_is_not_confirmed() {
        assert_eq!(composer_holds(OC_HOLDS, OC_SENT), Some(true));
    }

    #[test]
    fn a_gutter_drawn_composer_wrapping_a_batch_still_matches() {
        assert_eq!(composer_holds(OC_WRAPPED, OC_SENT), Some(true));
    }

    #[test]
    fn a_gutter_drawn_composer_too_short_to_classify_is_not_a_confirmation() {
        // Two words of ours on the composer, beside a model footer that is
        // five. Counted together they are long enough to be called somebody
        // else's; the footer is furniture and is not counted.
        assert_eq!(composer_holds(OC_SHORT, OC_SENT), None);
    }

    #[test]
    fn a_clear_gutter_drawn_composer_confirms_submission() {
        // A working lead pane carrying three transcript echoes of batches
        // it has already taken, each drawn in the same gutter as the
        // composer. Confirming here is half the point of the fix: an echo
        // read as a composer answers `Some(true)` on every tick, and the
        // unconfirmed streak skips a batch just as surely as a failed
        // delivery does. One region, and it is the composer.
        assert_eq!(composer_regions(OC_LIVE).len(), 1);
        assert_eq!(composer_holds(OC_LIVE, OC_SENT), Some(false));
    }

    #[test]
    fn furniture_right_aligned_over_the_composer_is_not_its_contents() {
        // Two clear composers in panes whose working directory is long
        // enough that OpenCode wraps it up the right margin, across the
        // rows the box is drawn on. `opencode-wrapped-cwd` is a live IC
        // pane, and it was found by running this locator over every pane in
        // the fleet rather than over the captures: read as composer
        // contents, those fragments are one word each, too short to
        // classify, and the pane is unconfirmable for as long as its
        // working directory is that long.
        for (name, pane) in [("an ic pane", OC_WRAPPED_CWD), ("a scratch pane", OC_EMPTY)] {
            assert_eq!(composer_regions(pane).len(), 1, "{name}");
            assert_eq!(composer_holds(pane, OC_SENT), Some(false), "{name}");
        }
    }

    #[test]
    fn an_idle_hint_over_a_clear_gutter_drawn_composer_confirms_submission() {
        // OpenCode's own `Ask anything...` hint, unlike a queue hint, is
        // shown *because* there is nothing to show: no queue stands behind
        // it, so a batch that reached the pane is not in one.
        assert_eq!(composer_holds(OC_HINT, OC_SENT), Some(false));
    }

    #[test]
    fn no_real_pane_holding_a_batch_reports_it_submitted() {
        // Requirement stated over the captures rather than over synthetic
        // perturbations: for every real pane of either kind with our text
        // unsubmitted on it, the one verdict that advances the cursor is
        // unreachable. `Some(true)` and `None` both cost a repeat.
        for (name, pane, sent) in [
            ("claude code, wrapped", HOLDS_BATCH, RULE),
            ("opencode", OC_HOLDS, OC_SENT),
            ("opencode, wrapped", OC_WRAPPED, OC_SENT),
            ("opencode, two words", OC_SHORT, OC_SENT),
            ("claude code, queued", PLACEHOLDER, RULE),
        ] {
            assert_ne!(
                composer_holds(pane, sent),
                Some(false),
                "{name}: a held batch reported submitted"
            );
        }
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
