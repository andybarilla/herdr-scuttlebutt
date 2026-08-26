use anyhow::{Context, Result};
use unicode_width::UnicodeWidthChar;

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
    /// The agent process's own session id (`agent_session.value`). `None`
    /// means `herdr agent list` did not emit it — for the agent kinds that
    /// never report one, and for a listing where it is momentarily missing.
    /// A restarted agent reports a new id, which is what lets the daemon
    /// tell "this pane is a fresh process" from "this pane went quiet". An
    /// absent id is not evidence either way; `tick` decides what each
    /// combination of absent and present means, and says why there.
    pub session: Option<String>,
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
///
/// A bare `>` was here and is not a marker. No agent this fleet runs opens
/// its composer with one — Claude Code draws `\u{276f}`, OpenCode draws no marker
/// at all — and `>` is what markdown opens a blockquote with and what a
/// shell draws for a continuation line. A marker that ordinary text also
/// starts with turns transcript into a composer, and a composer that is
/// really transcript answers for the pane.
const MARKERS: [&str; 2] = ["\u{276f}", "\u{203a}"];

/// Verticals an editor may draw down the left edge of its input box on
/// every row, in place of a rule above it. OpenCode draws the heavy one.
///
/// The light `\u{2502}` was here and is not one. It is what a rendered markdown
/// table draws its rows with, and a table cell that wraps puts two of them
/// straight above the table's own bottom rule — a gutter box, by every test
/// this applies. `gutter_bounded` has no second rule to measure a box
/// against, so nothing downstream would have caught it: the region is a
/// composer, and a cell quoting `DELIVERY_RULE` is a composer holding our
/// batch forever. Same defect as a bare `>` in `MARKERS`, one function
/// over. An editor that really did draw a light gutter would identify
/// nothing here, which costs a repeat.
const GUTTERS: [char; 1] = ['\u{2503}'];

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
/// The line has to open and close with box-drawing characters and carry an
/// unbroken run of `RULE_RUN` horizontals. Between them it may carry one
/// label: Claude Code writes the session title into the top border of its
/// composer, right-aligned rather than centred — `\u{2500}\u{2500}\u{2500} clear-conversation-state \u{2500}`
/// is 274 horizontals, the title, and one more — which is #36. Requiring
/// every character to be box-drawing rejected that border, left one rule in
/// the pane, and identified no composer at all.
///
/// The unbroken run is measured per run rather than summed, so that a short
/// centred label between two stubs — `\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}`, the shape a
/// pasted transcript actually carries — stays content. That is a bound, not
/// a guarantee: a body line carrying a long enough run and one label is
/// indistinguishable from a titled border, and reading it as furniture
/// splits the composer in two. Nothing in a line can settle that, so
/// `rule_bounded` keeps the half without the marker rather than discarding
/// it, and `composer_holds` still finds our text on it.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    let boxed = |c: char| is_horizontal(c) || is_joint(c);
    if !line.starts_with(boxed) || !line.ends_with(boxed) {
        return false;
    }
    // A gutter opens a row of a box; it never opens that box's edge. Left in,
    // `\u{2503}  \u{251c}\u{2500}\u{2500}\u{2500}\u{2524}` — a table separator inside a message OpenCode has
    // already taken — is a rule, and `gutter_bounded` walks up from it and
    // boxes the transcript above as a composer.
    if line.starts_with(|c| GUTTERS.contains(&c)) {
        return false;
    }
    // Horizontals per run of box-drawing characters, so a label breaks the
    // run it interrupts rather than being summed across.
    let mut runs = vec![0usize];
    let mut labels = 0;
    let mut in_label = false;
    // A gap is not a title. A line that puts two pieces of furniture side by
    // side — a gutter and a rule, a bordered cell holding a rule — separates
    // them with spaces, and reading the whole line as one rule moves a
    // region boundary onto a row that was never an edge.
    let mut blank_label = false;
    for c in line.chars() {
        if boxed(c) {
            if in_label {
                runs.push(0);
                in_label = false;
            }
            *runs.last_mut().expect("a run is always open") += usize::from(is_horizontal(c));
        } else if !in_label {
            in_label = true;
            labels += 1;
            blank_label = true;
        }
        if !boxed(c) {
            blank_label &= c.is_whitespace();
        }
    }
    labels <= 1 && !blank_label && runs.iter().any(|r| *r >= RULE_RUN)
}

/// What a pane's rules and gutters divide it into.
///
/// `composers` are the regions a marker or a gutter identified as one.
/// `others` are the regions beside a composer that carry no marker, and
/// they exist because a region boundary can move: a rule inside a message
/// body splits the composer, and the half carrying our text is then the
/// half without the marker. Those regions can say our text is still on the
/// composer, and their mere presence stops a composer claiming to be clear;
/// neither can they ever say one is. Discarding them, which is what this
/// did before, turned a moved boundary into `Some(false)` and dropped the
/// batch.
///
/// No captured pane produces an `other`, and the tests that reach one are
/// synthetic. Reaching one takes a rule inside the composer drawn at the
/// box's own column and width, and a real composer indents what it holds,
/// so a pasted rule lands a column or two in and matches nothing. Kept
/// anyway, deliberately: nothing else stands between that shape and a
/// dropped batch, what it costs when it does fire is a repeat rather than a
/// drop, and every layout this has been wrong about so far was one nobody
/// had captured either.
struct Regions {
    composers: Vec<Vec<Row>>,
    others: Vec<Vec<Row>>,
}

/// One row of an identified region, in the two forms the verdict needs.
///
/// They differ only where a clip ran, which is `gutter_bounded`: a
/// rule-bounded box spans the pane and is read whole, so `bare` and `text`
/// are the same string. Where they differ, the difference is exactly what a
/// measurement decided, which is what `composer_holds` refuses to let a row
/// vote on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    /// The row clipped to its box, its gutter or marker stripped, and
    /// normalized. What `is_our_text` reads and what a word count measures.
    text: String,
    /// The same row with only its indent and its gutter or marker removed,
    /// and no column arithmetic anywhere. Equal to `text` when the clip
    /// took nothing off the row.
    bare: String,
}

/// Every region of `pane` a composer could be, each as its own lines,
/// normalized and stripped of the marker or gutter that identified it.
///
/// Lines rather than one joined string because the two decisions want
/// different views of a composer. Whether it holds our text reads best
/// across the whole thing, since the composer wraps mid-sentence. Whether
/// it is *clear* has to read line by line: a box that prints furniture of
/// its own below the text — OpenCode's model footer — is text that is not
/// ours sitting beside a short unsubmitted prompt, and read as one string
/// the two together are long enough to classify.
///
/// The two layouts here are the two this fleet runs, and neither is a
/// guess: a rule-bounded box that opens with a known prompt marker, and a
/// gutter-bounded box closed by a rule. A layout matching neither
/// identifies nothing, which costs a repeat delivery.
fn composer_regions(pane: &str) -> Regions {
    let lines: Vec<&str> = pane.lines().collect();
    let mut regions = rule_bounded(&lines);
    regions.composers.extend(gutter_bounded(&lines));
    regions
}

/// Whether two rules could be the two edges of one box: drawn at the same
/// column, to the same width. A composer's borders are; a rule that arrived
/// inside a message body, or the border of a table in the transcript, has
/// no reason to match the composer's and in practice does not.
fn same_box(top: &str, bottom: &str) -> bool {
    box_span(top) == box_span(bottom)
}

/// Every rule-bounded region that could be a composer, split by whether it
/// opens with a prompt marker.
///
/// What has to be kept out is the transcript. A rendered markdown table
/// draws rules of its own, and this room's traffic is full of tables — the
/// region between two of those rules carries a row that begins with
/// `DELIVERY_RULE` and therefore matches every batch we ever send, forever.
/// Two separate tests keep it out, and they do different jobs. Measured on
/// `composer-below-a-table`, whose table separators all span `(2, 90)`:
///
/// - The table's own regions *do* match as boxes — a table is a box. What
///   excludes them is that neither sits beside a composer. Without that,
///   the pane answers `Some(true)` on a clear composer on every tick.
/// - Matching edges exclude the region between the table and the composer,
///   which spans two rules of different widths and would otherwise be an
///   `other` beside the composer, vetoing forever. Without it, the same
///   pane answers `None` on every tick.
///
/// Every qualifying region is returned, not just the last one, and that is
/// the point. A message body ending in a rule of its own, or a bordered box
/// drawn below the composer, both put a *different* region last; picking one
/// region by position is a guess, and a wrong guess here reports a batch
/// submitted that is sitting on a composer two lines up.
///
/// `others` — regions that qualify but carry no marker — are kept only next
/// to a composer, because that is the one place a composer's own content
/// can be: a rule that arrives inside the box splits it, and the half
/// holding our text is then the half without the marker. They can say our
/// text is still there; they can never say a composer is clear. Kept
/// further afield, they are the transcript again, vetoing forever.
fn rule_bounded(lines: &[&str]) -> Regions {
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_rule(l))
        .map(|(i, _)| i)
        .collect();
    let mut boxed: Vec<Option<(bool, Vec<Row>)>> = Vec::new();
    for w in rules.windows(2) {
        if !same_box(lines[w[0]], lines[w[1]]) {
            boxed.push(None);
            continue;
        }
        // No clip runs here — the box spans the pane — so each row reads
        // the same way whether or not a measurement is trusted.
        let mut region: Vec<Row> = lines[w[0] + 1..w[1]]
            .iter()
            .map(|l| normalize(l))
            .filter(|l| !l.is_empty())
            .map(|text| Row {
                bare: text.clone(),
                text,
            })
            .collect();
        let marker = region
            .first()
            .and_then(|first| MARKERS.iter().find(|m| first.text.starts_with(**m)))
            .map(|m| m.len());
        match marker {
            Some(marker) => {
                region[0].text = region[0].text[marker..].trim().to_string();
                region[0].bare = region[0].text.clone();
                boxed.push(Some((true, region)));
            }
            None if region.is_empty() => boxed.push(None),
            None => boxed.push(Some((false, region))),
        }
    }
    let composer_at = |i: usize| matches!(boxed.get(i), Some(Some((true, _))));
    let mut regions = Regions {
        composers: Vec::new(),
        others: Vec::new(),
    };
    for (i, region) in boxed.iter().enumerate() {
        match region {
            Some((true, lines)) => regions.composers.push(lines.clone()),
            Some((false, lines)) if composer_at(i + 1) || (i > 0 && composer_at(i - 1)) => {
                regions.others.push(lines.clone())
            }
            _ => {}
        }
    }
    regions
}

/// The columns a box occupies, read off the rule that closes it.
///
/// A pane right-aligns furniture of its own outside that edge — OpenCode
/// wraps a long working directory up the right margin, across the rows the
/// composer is drawn on — and clipping to the box is what keeps that out of
/// the composer's contents. Left in, it reads as text that is not ours: a
/// two-word unsubmitted prompt with a fragment of a path beside it is three
/// words, which is no longer too short to classify. That was `Some(false)`
/// and the batch gone, which is how every batch #36 lost was lost; since #47
/// a row this clipped anything off may not cast that vote either way, so
/// getting these columns wrong costs a repeat rather than the batch. The
/// clip still has to be right — it is what the contents *are*.
///
/// Columns are display cells rather than characters, because that is what
/// the pane was drawn in. Counting characters clips a row carrying a
/// double-width character late, by one column per such character, and the
/// fragment it keeps is exactly the furniture this exists to remove.
fn box_span(border: &str) -> (usize, usize) {
    let indent: String = border.chars().take_while(|c| c.is_whitespace()).collect();
    (display_width(&indent), display_width(border.trim_end()))
}

/// The cells one character is drawn in, given what the character before it
/// measured.
///
/// `U+FE0F` promotes what precedes it: `unicode-width` reports the variation
/// selector as zero and leaves `\u{26a0}` at one cell, while a terminal draws the
/// emoji presentation in two. It is the *only* character whose width depends
/// on its neighbour here, which is why the dependency is threaded through
/// rather than hidden — a column has to mean the same thing to everything
/// that measures one. Two metrics over one coordinate space is how a clip
/// keeps the furniture it exists to remove.
///
/// A selector with nothing to promote, or one after a character already two
/// cells wide, adds nothing. A second consecutive selector promotes again,
/// so `\u{26a0}\u{fe0f}\u{fe0f}` measures three where a terminal draws two. Left alone
/// deliberately: over-counting a row clips it early, which can only shorten
/// what a composer is read as holding, and shortening is the direction that
/// costs a repeat rather than a batch. Since #47 it costs even less — a row
/// the clip shortened is a row a measurement touched, so it may not say the
/// composer is clear at all, whichever direction the error ran.
fn cell_width(c: char, previous: usize) -> usize {
    match c {
        '\u{fe0f}' => usize::from(previous == 1),
        _ => c.width().unwrap_or(0),
    }
}

/// Display width in terminal cells, which is what a pane's columns are.
fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut previous = 0;
    for c in s.chars() {
        previous = cell_width(c, previous);
        width += previous;
    }
    width
}

/// One row of a gutter-bounded box, clipped to the box's columns, gutter
/// stripped and normalized. `bare_row` is the same row with the same
/// furniture stripped and no clip, and the two differ exactly where a
/// measurement had a hand in the row.
fn boxed_row(row: &str, (start, end): (usize, usize)) -> String {
    let mut col = 0;
    let mut previous = 0;
    let mut clipped = String::new();
    for c in row.chars() {
        if col >= end {
            break;
        }
        if col >= start {
            clipped.push(c);
        }
        previous = cell_width(c, previous);
        col += previous;
    }
    // Trimmed before the gutter is stripped, because the clip starts at the
    // rule's own indent and a box's rows may be indented further than the
    // rule that closes it.
    normalize(
        clipped
            .trim_start()
            .trim_start_matches(|c| GUTTERS.contains(&c)),
    )
}

/// The same row with its furniture stripped and nothing measured: the
/// indent and the gutter go, the columns are left alone.
///
/// This is what says whether a row is empty, and whether the clip above
/// took anything off it. Both questions have to be answerable without a
/// measurement, because a measurement is what #47 stops a row voting on.
fn bare_row(row: &str) -> String {
    normalize(
        row.trim_start()
            .trim_start_matches(|c| GUTTERS.contains(&c)),
    )
}

/// Every box drawn with a vertical down each of its rows and closed by a
/// rule beneath them. OpenCode draws its composer that way and puts a
/// single horizontal in the whole pane — the bottom edge of that box. One
/// rule bounds no region, which is the other half of #36, and no widening
/// of `is_rule` can reach it, because the rules it would need are not
/// drawn.
///
/// Closed by a rule, rather than sitting at the bottom of the pane: this
/// layout gives the composer no marker of its own and draws *submitted*
/// messages in the transcript inside the same gutter, so something has to
/// separate the two, and it is the rule underneath. An echo has the rest of
/// the transcript below it instead. The composer is not always the lowest
/// thing in the pane either — OpenCode centres its box on a fresh session,
/// with the transcript hint and status line below — so "lowest" would be
/// wrong as well as unpinned.
///
/// Every such box is returned rather than the last, for the same reason
/// `rule_bounded` returns every region: a bordered overlay drawn below the
/// composer is another box closed by a rule, and returning only the lowest
/// would replace the composer with the overlay and report the batch
/// submitted from under it.
fn gutter_bounded(lines: &[&str]) -> Vec<Vec<Row>> {
    let gutter = |l: &&str| l.trim_start().starts_with(|c| GUTTERS.contains(&c));
    let mut boxes = Vec::new();
    for (bottom, _) in lines.iter().enumerate().filter(|(_, l)| is_rule(l)) {
        let mut top = bottom;
        while top > 0 && gutter(&lines[top - 1]) {
            top -= 1;
        }
        // A box, not a single bordered line: one gutter row above a rule is
        // as easily a transcript echo that happens to end there.
        if bottom - top < 2 {
            continue;
        }
        let span = box_span(lines[bottom]);
        boxes.push(
            lines[top..bottom]
                .iter()
                .map(|l| Row {
                    text: boxed_row(l, span),
                    bare: bare_row(l),
                })
                .collect(),
        );
    }
    boxes
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
/// path: at least one composer was identified, and every one of its rows says
/// so on evidence no measurement produced. A row says so by being empty
/// before any clip, span or width calculation ran, or by carrying enough
/// words to be recognized as text that is not ours while the clip took
/// nothing off it. That is #47: a clip, a span or a width calculation cannot
/// manufacture the evidence of a clear composer, because neither thing a row
/// may vote on is anything they had a hand in.
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
///
/// What #47 costs, measured rather than guessed: an OpenCode pane whose
/// working directory wraps up the right margin across the box's rows can no
/// longer confirm anything, because the clip takes that fragment off a row
/// which then may not vote. Three captures are in that state
/// (`opencode-empty`, `opencode-live-room`, `opencode-wrapped-cwd`) and no
/// live pane was at the time of the change. Under #42 that holds the batch
/// and re-prompts it rather than dropping it.
///
/// A composer carrying text that is merely *someone else's* — a human
/// typing, OpenCode's `Ask anything...` hint — still confirms. #47 proposed
/// closing that too; it was deliberately left open, because none of the four
/// batches #36 lost went that way and closing it would stall every pane a
/// human is typing in for no measured safety.
fn composer_holds(pane: &str, sent: &str) -> Option<bool> {
    let Regions { composers, others } = composer_regions(pane);
    if composers.is_empty() {
        return None;
    }
    let sent = normalize(sent);
    let joined = |c: &Vec<Row>| {
        c.iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    if composers
        .iter()
        .chain(&others)
        .any(|c| is_our_text(&joined(c), &sent))
    {
        return Some(true);
    }
    // A region kept in `others` is a rule inside the box with content on
    // the far side of it, so which half is the composer's is exactly what
    // cannot be told apart. The marker half saying it is empty is not
    // evidence the box is: our text may be the half that lost the marker,
    // mangled past a three-word window and so unable to say so above.
    if !others.is_empty() {
        return None;
    }
    // Only a region something identified as a composer can say a composer
    // is clear. `others` has already had its say above.
    let rows = || composers.iter().flatten();
    // A composer showing a queue hint is showing neither our text nor a
    // clear box: the queue it names may hold our batch or may not, and
    // nothing in the pane says which.
    if rows().any(|r| QUEUE_HINTS.iter().any(|h| r.text.contains(h))) {
        return None;
    }
    // A row may say a composer is clear only on evidence no measurement
    // could have manufactured. Either it is empty before any clip, span or
    // width calculation ran, or it carries enough words to be recognized as
    // text that is not ours *and* the clip took nothing off it, so no width
    // calculation contributed one of those words.
    //
    // Per row, because a composer that draws furniture of its own below the
    // text — OpenCode's model footer — would otherwise carry every short
    // row past this on the strength of the furniture's word count.
    let says_clear = |r: &Row| {
        if r.bare.is_empty() {
            return true;
        }
        let words = r
            .text
            .trim_end_matches(['\u{2026}', '.', ' '])
            .split_whitespace()
            .count();
        words >= OVERLAP_WORDS && r.bare == r.text
    };
    match rows().all(says_clear) {
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
                session: a["agent_session"]["value"].as_str().map(str::to_string),
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
    const TABLE: &str = include_str!("../tests/fixtures/composer-below-a-table.txt");
    const WRAPPED_TABLE: &str =
        include_str!("../tests/fixtures/composer-below-a-wrapped-table.txt");

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
        // `MAX_FAILURES_BEFORE_STALL` still bounds it: at the cap the
        // agent stalls, which holds the batch, leaves the cursor where
        // it is, drops to a widening retry and names the agent in
        // `daemon-status` (#42). So the cost is bounded and logged, and
        // nothing is lost while it runs; this was a silent drop.
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
        // skipped the batch after five tries. That was then — #42 replaced
        // skipping with stall-and-hold, so an unconfirmable pane now holds
        // its batch and re-prompts it on a widening wait (#39). Five is the
        // historical number and stays five here whatever the constant does.
        for (name, pane) in [
            ("claude code, titled border", TITLED),
            ("claude code, plain border", EMPTY),
            ("opencode, clear", OC_EMPTY),
            ("opencode, holding a batch", OC_HOLDS),
            ("opencode, a working lead", OC_LIVE),
            ("opencode, a wrapped working directory", OC_WRAPPED_CWD),
            ("claude code, a table above the composer", TABLE),
        ] {
            assert!(
                !composer_regions(pane).composers.is_empty(),
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
        // The real titled pane with the real held rows written into its
        // composer. Both of its borders are kept, so the box still measures
        // as one: splicing only the top border in would have changed the
        // box's width and tested nothing but the mismatch.
        let mut lines: Vec<&str> = TITLED.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.contains("clear-conversation-state"))
            .expect("fixture lost its titled border");
        let bottom = top
            + lines[top..]
                .iter()
                .skip(1)
                .position(|l| is_rule(l))
                .expect("fixture lost its composer")
            + 1;
        let batch: Vec<&str> = HOLDS_BATCH
            .lines()
            .skip_while(|l| !l.starts_with('\u{276f}'))
            .take_while(|l| !is_rule(l))
            .collect();
        lines.splice(top + 1..bottom, batch);
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
    fn an_echo_in_the_composers_own_gutter_is_not_a_composer() {
        // A working lead pane carrying three transcript echoes of batches
        // it has already taken, each drawn in the same gutter as the
        // composer. Identifying exactly one region is what this pins: an
        // echo read as a composer answers `Some(true)` on every tick, and
        // the unconfirmed streak skips a batch just as surely as a failed
        // delivery does.
        //
        // The verdict is `None` where this pane gave `Some(false)` before
        // #47. Its footer row carries the working directory wrapped up the
        // right margin, so the clip took something off the only row that
        // could have voted, and a row a measurement had a hand in may no
        // longer say a composer is clear —
        // `furniture_right_aligned_over_the_composer_stops_it_confirming`
        // is where that is argued.
        //
        // What excludes them here is that no rule is drawn beneath any of
        // them. That is a property of this pane, not a guarantee, and two
        // rounds of this comment claiming otherwise is why it is spelled
        // out. A rule inside an echo's own gutter row — a table or a `---`
        // in a message body — is covered, by
        // `a_rule_inside_a_gutter_row_does_not_box_the_transcript`. A rule
        // drawn on its own line directly beneath an echo's block is not:
        // inserting one line of `\u{2579}\u{2580}\u{2580}\u{2580}...` below an echo here yields that
        // echo as a region holding our batch, and `Some(true)` on a clear
        // composer for as long as the pane stands. Elided — as written
        // those are three horizontals, under `RULE_RUN`, so pasting them
        // into a test reproduces nothing; the real line is the width of
        // the pane.
        //
        // Accepted rather than closed. No capture shows a rule beside an
        // echo, every OpenCode pane we have carries exactly one rule, the
        // shape needs no blank row between the two blocks, and what it
        // costs is the bounded `Some(true)` — a stall that holds the batch
        // — rather than a drop.
        //
        // Note what does *not* help: `composer_holds` returns on the first
        // region matching our text, so keeping every box rather than the
        // lowest never enters into this. That serves the opposite case, an
        // overlay drawn below a real composer.
        assert_eq!(composer_regions(OC_LIVE).composers.len(), 1);
        assert_eq!(composer_holds(OC_LIVE, OC_SENT), None);
    }

    #[test]
    fn furniture_right_aligned_over_the_composer_stops_it_confirming() {
        // Two clear composers in panes whose working directory is long
        // enough that OpenCode wraps it up the right margin, across the
        // rows the box is drawn on. `opencode-wrapped-cwd` is a live IC
        // pane, and it was found by running this locator over every pane in
        // the fleet rather than over the captures.
        //
        // The clip still keeps those fragments out of the contents: one
        // composer is identified and its rows are what the box holds. What
        // #47 changed is which rows may vote on that reading. A row the
        // clip took something off had a width calculation decide part of
        // what it says, and every batch #36 lost was that decision going
        // wrong — a fragment kept, a short row of ours padded to three
        // words that match nothing we sent, and a composer read as clear.
        // So these panes confirm nothing for as long as their working
        // directory is that long. Under #42 that holds the batch and
        // re-prompts it rather than dropping it.
        for (name, pane) in [("an ic pane", OC_WRAPPED_CWD), ("a scratch pane", OC_EMPTY)] {
            assert_eq!(composer_regions(pane).composers.len(), 1, "{name}");
            assert_eq!(composer_holds(pane, OC_SENT), None, "{name}");
        }
    }

    #[test]
    fn an_idle_hint_over_a_clear_gutter_drawn_composer_confirms_submission() {
        // OpenCode's own `Ask anything...` hint, unlike a queue hint, is
        // shown *because* there is nothing to show: no queue stands behind
        // it, so a batch that reached the pane is not in one.
        //
        // This pane still confirms where the other three OpenCode captures
        // no longer do, and the difference is not the hint: nothing on this
        // box's rows is clipped, so every row still says what it says
        // without a measurement. #47 proposed making a composer carrying
        // someone else's text `None` as well; that half was deliberately
        // not done, so the hint reads exactly as it did before.
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

    #[test]
    fn a_table_in_the_transcript_is_not_a_composer() {
        // A real Claude Code pane with a rendered markdown table above a
        // clear composer, one of whose rows carries `DELIVERY_RULE` — which
        // opens every batch we send, so it matches all of them, forever.
        // Read without matching the box's edges, this pane answers
        // `Some(true)` on every tick and the streak skips the batch: #36's
        // symptom, from the fix for #36.
        assert!(
            TABLE.lines().filter(|l| is_rule(l)).count() > 2,
            "fixture lost its table"
        );
        assert_eq!(composer_regions(TABLE).composers.len(), 1);
        assert!(composer_regions(TABLE).others.is_empty());
        assert_eq!(composer_holds(TABLE, RULE), Some(false));
    }

    #[test]
    fn an_emoji_presentation_label_measures_the_cells_it_is_drawn_in() {
        // A composer's two rules are measured independently and have to
        // agree. `unicode-width` reports `\u{26a0}\u{fe0f}` as one cell and a terminal
        // draws two, so a title carrying one would make the top border
        // measure narrower than the bottom, pair with nothing, and identify
        // no composer for as long as the session kept that title.
        assert_eq!(display_width("\u{26a0}\u{fe0f}"), 2);
        let run = "\u{2500}".repeat(RULE_RUN);
        let titled = format!("{run} \u{26a0}\u{fe0f} {run}");
        let plain = "\u{2500}".repeat(display_width(&titled));
        assert!(same_box(&titled, &plain));
        let pane = format!("{titled}\n\u{276f} Reply only if you have\n{plain}\n  status");
        assert_eq!(composer_holds(&pane, RULE), Some(true));
    }

    #[test]
    fn a_row_and_its_box_are_measured_the_same_way() {
        // `box_span` promoted an emoji-presentation sequence to the two
        // cells a terminal draws it in and `boxed_row` did not, so the clip
        // ran a cell late per sequence and kept the start of the working
        // directory wrapped up the right margin. Beside two words of ours
        // that is a third word, and a third word classifies: `Some(false)`
        // on a prompt still sitting on the composer.
        //
        // `\u{26a0}\u{fe0f}` and not `\u{1f389}`, because the two metrics have to disagree
        // about the character for the test to see the difference between
        // them — `\u{1f389}` is two cells under both.
        assert_ne!(
            display_width("\u{26a0}\u{fe0f}"),
            "\u{26a0}\u{fe0f}"
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>(),
            "the metrics agree, so this proves nothing"
        );
        let mut lines: Vec<String> = OC_WRAPPED_CWD.lines().map(String::from).collect();
        let bottom = lines
            .iter()
            .position(|l| is_rule(l))
            .expect("fixture lost its box");
        let span = box_span(&lines[bottom]);
        let held = "  \u{2503}  Reply\u{26a0}\u{fe0f} only";
        lines[bottom - 3] = format!(
            "{held}{}~/.herdr/worktrees/alare",
            " ".repeat(span.1 - display_width(held))
        );
        assert_eq!(
            boxed_row(&lines[bottom - 3], span),
            "Reply\u{26a0}\u{fe0f} only"
        );
        assert_eq!(composer_holds(&lines.join("\n"), OC_SENT), None);
    }

    #[test]
    fn a_rule_inside_a_gutter_row_does_not_box_the_transcript() {
        // OpenCode draws messages it has already taken in the same gutter as
        // its composer, and a message body carrying a table or a `---` puts a
        // rule inside one of those rows. Read as a rule, `gutter_bounded`
        // walks up from it and boxes the echo above as a composer — holding
        // the batch it quotes, on every tick, forever.
        //
        // That is why `a_clear_gutter_drawn_composer_confirms_submission`
        // could not carry the guarantee on its own: "no rule is drawn beneath
        // an echo" is false for any message body containing one.
        let separator = format!("  \u{2503}  \u{251c}{}\u{2524}", "\u{2500}".repeat(16));
        let plain = format!("  \u{2503}  {}", "\u{2500}".repeat(16));
        // Two guards keep those out, and each has a shape only it catches.
        // A gutter with the run against it carries no label at all; a
        // bordered table cell holding a rule is not gutter-led.
        let run = "\u{2500}".repeat(RULE_RUN * 2);
        assert!(!is_rule(&format!("\u{2503}{run}")), "gutter against a run");
        assert!(
            !is_rule(&format!("\u{2502}  {run}")),
            "a cell holding a rule"
        );
        for injected in [separator, plain] {
            assert!(!is_rule(&injected), "{injected:?} read as a rule");
            let pane: Vec<String> = OC_EMPTY
                .lines()
                .map(|l| match l.contains("[scuttlebutt] New messages") {
                    true => injected.clone(),
                    false => l.to_string(),
                })
                .collect();
            let pane = pane.join("\n");
            assert_eq!(composer_regions(&pane).composers.len(), 1);
            // `None` rather than a confirmation because this is built on
            // `opencode-empty`, whose composer rows carry the wrapped
            // working directory; the region count is what the injected
            // rule is being tested against.
            assert_eq!(composer_holds(&pane, OC_SENT), None);
        }
    }

    #[test]
    fn both_ends_of_a_box_span_are_measured_in_cells() {
        // The sibling of the two-metric defect, in the same function: the
        // indent was counted in characters while the far edge was measured
        // in cells.
        //
        // These two rules are drawn at different columns, and counting the
        // indent is exactly what hides it. An ideographic space is one
        // character and two cells, so under the old metric both spans came
        // out (1, 10) — same start, same end — and `same_box` called them
        // one box, pairing a rule that is not the composer's edge with it.
        // The pair matters: two rules differing in both fields would fail
        // `same_box` under either metric and prove nothing.
        let wide = format!("\u{3000}{}", "\u{2500}".repeat(8));
        let narrow = format!(" {}", "\u{2500}".repeat(9));
        assert!(!same_box(&wide, &narrow));
        assert_eq!(box_span(&wide), (2, 10));
        assert_eq!(box_span(&narrow), (1, 10));
    }

    #[test]
    fn a_variation_selector_with_nothing_to_promote_measures_nothing() {
        assert_eq!(display_width("\u{fe0f}"), 0);
        assert_eq!(display_width("\u{26a0}\u{fe0f}"), 2);
        // Already two cells; the selector adds nothing to it.
        assert_eq!(display_width("\u{1f389}\u{fe0f}"), 2);
    }

    /// A pane's own composer rows replaced with the batch, keeping both of
    /// its borders so the box still measures as one.
    fn holding(pane: &str) -> String {
        let mut lines: Vec<&str> = pane.lines().collect();
        let top = lines
            .iter()
            .rposition(|l| is_rule(l))
            .and_then(|bottom| lines[..bottom].iter().rposition(|l| is_rule(l)))
            .expect("pane lost its composer");
        let bottom = top
            + lines[top..]
                .iter()
                .skip(1)
                .position(|l| is_rule(l))
                .expect("pane lost its composer")
            + 1;
        let batch: Vec<&str> = HOLDS_BATCH
            .lines()
            .skip_while(|l| !l.starts_with('\u{276f}'))
            .take_while(|l| !is_rule(l))
            .collect();
        lines.splice(top + 1..bottom, batch);
        lines.join("\n")
    }

    #[test]
    fn a_table_pane_holding_a_batch_is_not_confirmed() {
        // The cross-product the suite was missing. Tables were tested only
        // against a clear composer and held batches only against
        // table-free panes, so the one shape nobody covered was the one
        // where a moved boundary costs a batch rather than an agent: a
        // pane whose transcript draws rules *and* whose composer still
        // holds us.
        for (name, pane) in [
            ("a rendered table", TABLE),
            ("a table with a wrapped cell", WRAPPED_TABLE),
        ] {
            let held = holding(pane);
            let regions = composer_regions(&held);
            assert_eq!(regions.composers.len(), 1, "{name}");
            assert!(regions.others.is_empty(), "{name}");
            assert_eq!(composer_holds(&held, RULE), Some(true), "{name}");
        }
    }

    #[test]
    fn a_wrapped_table_cell_is_not_a_gutter_composer() {
        // A real Claude Code pane, narrow enough that a table cell wraps,
        // so three light-vertical rows sit straight above the table's own
        // bottom rule. That is a gutter box by every test `gutter_bounded`
        // applies, and it has no second rule for `same_box` to measure — so
        // the cell, which quotes `DELIVERY_RULE`, is a composer holding our
        // batch on every tick. Read with the light vertical still a gutter,
        // this pane gives two composers and `Some(true)`.
        assert!(WRAPPED_TABLE.contains('\u{2502}'), "fixture lost its table");
        assert_eq!(composer_regions(WRAPPED_TABLE).composers.len(), 1);
        assert_eq!(composer_holds(WRAPPED_TABLE, RULE), Some(false));
    }

    #[test]
    fn two_rules_ending_at_one_column_are_not_one_box() {
        // `box_span` reads both edges of the box, and both are load-bearing.
        // A rule quoted into the batch, indented and shorter, can end at
        // exactly the column the composer's border ends at — comparing only
        // where they end pairs the two and splits the box between them.
        let rule = "\u{2500}".repeat(60);
        let inset = format!("    {}", "\u{2500}".repeat(56));
        assert_eq!(box_span(&rule).1, box_span(&inset).1);
        assert!(same_box(&rule, &rule));
        assert!(!same_box(&rule, &inset));
        let composer = ["\u{276f}".to_string(), inset, "  Done.".to_string()];
        assert_eq!(holds(&composer, RULE), None);
    }

    #[test]
    fn a_blockquote_is_not_a_prompt_marker() {
        // `>` opens a markdown blockquote and a shell continuation line as
        // readily as it opens anyone's composer, and no agent this fleet
        // runs opens one with it. Read as a marker, a quoted line between
        // two rules of matching width is a composer holding whatever it
        // quotes.
        let quoted = ["> Reply only if you have information others don't".to_string()];
        assert_eq!(holds(&quoted, RULE), None);
    }

    #[test]
    fn a_closing_rule_with_a_wide_label_measures_its_box_in_cells() {
        // `box_span` reads the box's columns off the rule that closes it.
        // Counted in characters, a double-width label makes that rule
        // measure narrower than it is drawn, and the rows above are clipped
        // short — far enough, on a narrow box, to cut our text off
        // entirely and leave rows that are all empty, which reads as clear.
        let run = "\u{2500}".repeat(RULE_RUN);
        let bottom = format!("{run} \u{6982}\u{8981} {run}");
        let pane = format!("  \u{2503}\n  \u{2503}  Reply only\n  \u{2503}\n{bottom}\n  status");
        assert_eq!(box_span(&bottom), (0, display_width(&bottom)));
        assert_ne!(composer_holds(&pane, RULE), Some(false));
    }

    // ---- a boundary that moved never loses the batch --------------------

    #[test]
    fn a_short_centred_label_between_stubs_is_not_a_rule() {
        // The shape a pasted transcript carries. Summing horizontals across
        // the label instead of measuring the run would make this furniture,
        // and furniture inside the composer splits it.
        assert!(!is_rule("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}"));
        assert!(is_rule(&format!(
            "{} Context \u{2500}",
            "\u{2500}".repeat(RULE_RUN)
        )));
    }

    #[test]
    fn a_line_with_two_labels_is_not_a_rule() {
        // One label is the session title. Two is a table row, a progress
        // bar, or a body line, and each label it is allowed moves the bound
        // further from anything a composer border does.
        let run = "\u{2500}".repeat(RULE_RUN);
        assert!(is_rule(&format!("{run} one {run}")));
        assert!(!is_rule(&format!("{run} one {run} two {run}")));
    }

    #[test]
    fn a_rule_inside_the_batch_does_not_hide_the_half_without_the_marker() {
        // A rule pasted into the batch at exactly the box's width splits it
        // into two regions that both look like the box's own. The marker
        // half here is bare, and read alone it is a cleared composer:
        // `Some(false)`, cursor advanced, batch gone. The half without the
        // marker is what says otherwise.
        let rule = "\u{2500}".repeat(60);
        let composer = [
            "\u{276f}".to_string(),
            rule,
            "Reply only if you have information others don't".to_string(),
        ];
        assert_eq!(holds(&composer, RULE), Some(true));
    }

    #[test]
    fn a_bare_marker_beside_a_split_box_does_not_confirm() {
        // The same split, with a tail too short for a three-word window to
        // recognize as ours. Nothing can say the text is still there, so
        // nothing may say it is gone either.
        let rule = "\u{2500}".repeat(60);
        let composer = ["\u{276f}".to_string(), rule, "  Done.".to_string()];
        assert_eq!(holds(&composer, RULE), None);
    }

    #[test]
    fn a_body_rule_of_another_width_identifies_no_box() {
        // The same shape with the pasted rule at a different width and
        // indent, which is what a pasted rule usually looks like. No two
        // rules in the pane can be one box's edges, so nothing is
        // identified at all.
        let composer = [
            "\u{276f}".to_string(),
            format!("    {}", "\u{2500}".repeat(16)),
            "  Done.".to_string(),
        ];
        assert_eq!(holds(&composer, RULE), None);
    }

    #[test]
    fn an_overlay_below_a_gutter_composer_does_not_hide_the_batch() {
        // A bordered box drawn under an OpenCode composer is another
        // gutter box closed by a rule. Taking only the lowest one replaces
        // the composer with the overlay and reports the batch submitted
        // from under it.
        let overlay = format!(
            "  \u{2503}\n  \u{2503}  Allow this command? [y/N]\n  \u{2503}\n{}",
            "\u{2500}".repeat(60)
        );
        let pane = format!("{OC_HOLDS}{overlay}");
        assert_eq!(composer_holds(&pane, OC_SENT), Some(true));
    }

    #[test]
    fn a_single_gutter_row_above_a_rule_is_not_a_composer() {
        // One bordered row is as easily the last line of a transcript echo
        // that happens to end above a rule. Read as a composer it is four
        // words that are not ours, which is a confirmation.
        let pane = format!(
            "  \u{2503}  someone else was here\n{}\n  status",
            "\u{2500}".repeat(60)
        );
        assert_eq!(composer_holds(&pane, RULE), None);
    }

    #[test]
    fn a_gutter_composer_is_found_above_the_bottom_of_the_pane() {
        // OpenCode centres its box on a fresh session, with a transcript
        // hint and a status line below it. The rule beneath the rows is
        // what closes the box; being lowest in the pane is not.
        let rule = OC_HINT
            .lines()
            .position(is_rule)
            .expect("fixture lost its box");
        assert!(
            rule < OC_HINT.lines().count() - 2,
            "fixture no longer has content below the box"
        );
        assert_eq!(composer_regions(OC_HINT).composers.len(), 1);
    }

    #[test]
    fn a_double_width_character_does_not_pull_furniture_into_a_row() {
        // Clipping by characters rather than display cells runs one column
        // late per double-width character, and what it keeps is the first
        // character of the working directory wrapped up the right margin.
        // Beside two words of ours that fragment is a third word, which is
        // enough to classify — and classified, an unsubmitted prompt is
        // submitted. Built on the real pane that wraps its cwd, with one
        // row of the box replaced.
        let mut lines: Vec<String> = OC_WRAPPED_CWD.lines().map(String::from).collect();
        let bottom = lines
            .iter()
            .position(|l| is_rule(l))
            .expect("fixture lost its box");
        let span = box_span(&lines[bottom]);
        let held = "  \u{2503}  Reply\u{1f389} only";
        let furniture = "~/.herdr/worktrees/alare/issue-1076:";
        lines[bottom - 3] = format!(
            "{held}{}{furniture}",
            " ".repeat(span.1 - display_width(held))
        );
        let pane = lines.join("\n");

        assert_eq!(boxed_row(&lines[bottom - 3], span), "Reply\u{1f389} only");
        assert_eq!(composer_holds(&pane, OC_SENT), None);
    }

    #[test]
    fn a_box_indented_further_than_its_closing_rule_still_reads() {
        // The clip starts at the rule's own indent, so a box whose rows are
        // indented past it keeps its gutter character as content. Every row
        // is then one word, nothing classifies, and the pane is
        // unconfirmable for as long as it is drawn that way.
        let span = box_span(&"\u{2500}".repeat(60));
        assert_eq!(boxed_row("    \u{2503}  Reply only", span), "Reply only");
    }

    #[test]
    fn a_gutter_box_without_a_footer_still_holds_what_is_in_it() {
        // The model footer used to be popped off the last row on its shape
        // alone — non-empty, blank row above it. A composer holding two
        // words has exactly that shape, and popping it left rows that were
        // all empty, which is a cleared composer. Per-row classification
        // does the work the pop was there for, so there is nothing to pop.
        let pane = format!(
            "  \u{2503}\n  \u{2503}\n  \u{2503}  Reply only\n{}\n  status",
            "\u{2500}".repeat(60)
        );
        assert_eq!(composer_holds(&pane, RULE), None);
    }

    /// One row of an OpenCode-shaped box: the gutter, the row's contents,
    /// then whatever the pane draws up its right margin outside the box.
    /// The box is `BOX_WIDTH` cells wide, so `margin` is what the clip is
    /// there to remove.
    fn gutter_row(content: &str, margin: &str) -> String {
        let body = format!("  \u{2503}  {content}");
        format!(
            "{body}{}{margin}",
            " ".repeat(BOX_WIDTH - display_width(&body))
        )
    }

    fn gutter_box(rows: &[String]) -> String {
        let mut lines = rows.to_vec();
        lines.push("\u{2500}".repeat(BOX_WIDTH));
        lines.push("  status".into());
        lines.join("\n")
    }

    const BOX_WIDTH: usize = 60;

    /// Every identified composer's rows as the clip leaves them.
    fn texts(pane: &str) -> Vec<Vec<String>> {
        composer_regions(pane)
            .composers
            .iter()
            .map(|c| c.iter().map(|r| r.text.clone()).collect())
            .collect()
    }

    #[test]
    fn a_row_the_clip_left_alone_reads_the_same_either_way() {
        // `bare_row` and `boxed_row` strip the same furniture off the same
        // row and have to do it the same way, so that the only thing left
        // between them is the clip. Two functions reading one row two ways
        // is #36's two-metrics defect one function over, and here it would
        // be silent in the expensive direction: every row would read as one
        // the clip had touched, and no gutter-drawn composer could confirm
        // anything again.
        let span = box_span(&"\u{2500}".repeat(BOX_WIDTH));
        for row in [
            "  \u{2503}",
            "  \u{2503}  Reply only",
            "    \u{2503}  indented further than its own closing rule",
            "  \u{2503}  Build \u{b7} GPT-5.6 Sol OpenAI",
        ] {
            assert_eq!(bare_row(row), boxed_row(row, span), "{row:?}");
        }
    }

    #[test]
    fn a_row_the_clip_touched_cannot_say_a_composer_is_clear() {
        // The measurement layer's actual mechanism, from #36: the clip kept
        // a fragment of the right margin, which padded a row up to a word
        // count that reads as text long enough to be recognized as not
        // ours — and a composer holding only text that is not ours is a
        // composer we call clear. Whether the clip got those columns right
        // or wrong, a row it took something off is a row whose word count
        // a width calculation had a hand in, so it may not cast that vote.
        let footer = "Build \u{b7} GPT-5.6 Sol OpenAI";
        let clean = gutter_box(&[
            gutter_row("", ""),
            gutter_row("", ""),
            gutter_row(footer, ""),
        ]);
        let overlapped = gutter_box(&[
            gutter_row("", ""),
            gutter_row("", ""),
            gutter_row(footer, "~/dev/alare:main"),
        ]);
        // Identical inside the box: the clip removes the difference, and
        // what is left is the same rows read the same way.
        assert_eq!(texts(&clean), texts(&overlapped));
        assert_eq!(composer_holds(&clean, RULE), Some(false));
        assert_eq!(composer_holds(&overlapped, RULE), None);
    }

    #[test]
    fn a_row_blank_only_after_clipping_is_not_an_empty_row() {
        // The other half of the same rule. An empty row is the evidence
        // that a composer is clear, so it has to be evidence no clip, span
        // or width calculation could have manufactured: the row is empty
        // only if it is empty before any of them ran.
        let footer = "Build \u{b7} GPT-5.6 Sol OpenAI";
        let clean = gutter_box(&[
            gutter_row("", ""),
            gutter_row("", ""),
            gutter_row(footer, ""),
        ]);
        let overlapped = gutter_box(&[
            gutter_row("", "~/dev/alare:main"),
            gutter_row("", "~/dev/alare:main"),
            gutter_row(footer, ""),
        ]);
        assert_eq!(texts(&clean), texts(&overlapped));
        assert_eq!(composer_holds(&clean, RULE), Some(false));
        assert_eq!(composer_holds(&overlapped, RULE), None);
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
        // The message's own rule splits the composer in two. Drawn to a
        // different width than the box, no pair of rules in the pane can be
        // that box's edges, so nothing is identified and the delivery
        // repeats. Drawn to the same width, the half without the marker is
        // kept beside the half with it, and it still holds us.
        let narrow = held(&[&"\u{2500}".repeat(30)]);
        assert_eq!(holds(&narrow, RULE), None);
        let matching = held(&[&"\u{2500}".repeat(60)]);
        assert_eq!(holds(&matching, RULE), Some(true));
    }

    #[test]
    fn a_one_character_tail_below_a_body_rule_is_not_a_confirmation() {
        // A tail too short to be recognized as ours, below a rule that
        // arrived inside the batch. Whether the rule matches the box or not,
        // the answer may never be that the composer is clear.
        let narrow = held(&[&"\u{2500}".repeat(30), "  -"]);
        assert_eq!(holds(&narrow, RULE), None);
        let matching = held(&[&"\u{2500}".repeat(60), "  -"]);
        assert_eq!(holds(&matching, RULE), Some(true));
        // The rule a pasted transcript actually carries: one short label
        // between two stubs, which is not furniture at all, so the composer
        // is never split and still reads as holding us.
        let titled = held(&["\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} Context \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}"]);
        assert_eq!(holds(&titled, RULE), Some(true));
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
