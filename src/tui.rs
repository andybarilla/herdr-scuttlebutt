use crate::groups::{CurrentRoom, Grouping, Room};
use crate::herd::{AgentInfo, HerdControl, RealHerd};
use crate::log_store::{self, Message};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

/// One row of the room picker, built once when the picker opens.
///
/// Everything a row displays is settled here rather than at draw time: the
/// unread dot would otherwise stat a file on every keystroke, and `sources`
/// would be a second derivation of the provenance `Room` already sorted by
/// — the drift that `Room::sources` exists to make impossible.
pub struct PickerRow {
    pub room: CurrentRoom,
    pub agents: usize,
    /// `Room::sources` labels, primary first, joined for display.
    pub sources: String,
    /// The room's `room.jsonl` has grown since this pane started. Never set
    /// for the room being viewed, whose log grows past its seed as you read.
    pub unread: bool,
}

/// The modal room list. Its presence is the pane's only mode: `handle_key`
/// returns early into `handle_picker_key` while it is `Some`.
pub struct PickerState {
    /// Rebuilt on every open, `herdr agent list` subprocess included. It
    /// runs on a human keystroke, so the cost is invisible and the list is
    /// fresh at the only moment anyone reads it.
    pub rows: Vec<PickerRow>,
    /// Case-insensitive substring, not prefix: group names here share stems
    /// and hyphens, so `scuttle` has to find `herdr-scuttlebutt`.
    pub filter: String,
    /// Index into the *filtered* rows. Every mutation of `filter` resets it,
    /// so it can never point past a narrowed list or at a row the human did
    /// not put the cursor on.
    pub cursor: usize,
}

impl PickerState {
    fn matches(&self) -> Vec<&PickerRow> {
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|r| r.room.label().to_lowercase().contains(&needle))
            .collect()
    }

    fn selected(&self) -> Option<CurrentRoom> {
        self.matches().get(self.cursor).map(|r| r.room.clone())
    }
}

/// What a keystroke asks the run loop to do. The picker's work — listing
/// agents, reading another room's log — is IO that `handle_key` cannot do
/// and that tests should not need a terminal for, so keys name the action
/// and `run` performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Post(String),
    OpenPicker,
    Switch(CurrentRoom),
}

#[derive(Default)]
pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub members: Vec<AgentInfo>,
    pub scroll_from_bottom: u16,
    pub quit: bool,
    /// The last failed action, surfaced in the input-line title. A transient
    /// failure must not tear the chat pane down — neither a write, nor a
    /// switch, nor a read of the room's log.
    ///
    /// Each writer stores a message describing its own action, because
    /// several actions land here and a fixed "post failed" prefix would
    /// report a failed switch as a post nobody attempted.
    pub last_error: Option<String>,
    /// Message-pane title, always naming the room being viewed — a group,
    /// the ungrouped room, or none selected. It is half the safeguard
    /// against posting into the wrong company's room; the input line's
    /// away-from-home marker is the other half, because that is where a
    /// post is actually committed.
    pub title: String,
    /// The room being viewed, and the one a post goes to.
    pub room: CurrentRoom,
    /// The room this pane opened in, `--group` included. Never recomputed
    /// from the live cwd: a pane opened with `--group` would then be
    /// permanently "away" from a home it is sitting in, inverting the
    /// away-from-home marker on the input border.
    pub home: CurrentRoom,
    /// `room`'s directory, `None` while no room is selected.
    pub dir: Option<PathBuf>,
    pub picker: Option<PickerState>,
    /// Unsent input per room, stashed on switch-out and restored on
    /// switch-in. Without it a draft either follows you into the next room
    /// or is silently dropped.
    pub drafts: HashMap<CurrentRoom, String>,
    /// Whether the message in `last_error` is the tail read's own, so that a
    /// later successful read clears that and nothing else. The tail runs at
    /// the top of every loop, before the draw, so an unconditional clear
    /// would wipe a `post failed: …` written after the last draw before the
    /// human ever saw it.
    ///
    /// It is a claim about who wrote `last_error`, which means every other
    /// writer has to clear it — `post` and `switch_room` both do, and a
    /// third writer added without doing so silently re-opens exactly that
    /// hole.
    pub tail_failed: bool,
    /// `room.jsonl` length per room when this pane started, refreshed for a
    /// room as you leave it. An unread dot is this compared against the
    /// file's length now, so it means "arrived while this pane has been
    /// open" rather than "history exists".
    pub unread_seeds: HashMap<CurrentRoom, u64>,
}

pub fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
    // The pane's one mode check. A branch per key would leave whichever key
    // was added next routing into `app.input` behind an open modal.
    if app.picker.is_some() {
        return handle_picker_key(app, code, modifiers);
    }
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
            app.quit = true;
            None
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) => Some(Action::OpenPicker),
        (KeyCode::Enter, _) => {
            // Nowhere to post to, and clearing the input would discard what
            // was typed with nothing written anywhere.
            if app.room.selected().is_none() || app.input.trim().is_empty() {
                None
            } else {
                let text = std::mem::take(&mut app.input);
                app.scroll_from_bottom = 0;
                Some(Action::Post(text))
            }
        }
        (KeyCode::Backspace, _) => {
            app.input.pop();
            None
        }
        (KeyCode::Up, _) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_add(1);
            None
        }
        (KeyCode::Down, _) => {
            app.scroll_from_bottom = app.scroll_from_bottom.saturating_sub(1);
            None
        }
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            app.input.push(c);
            None
        }
        _ => None,
    }
}

/// Keys while the room picker is open. `Esc` closes the modal instead of
/// quitting the pane — safe only because the modal is on screen; an
/// unconditional quit would train people to lose the pane.
fn handle_picker_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.quit = true;
            None
        }
        (KeyCode::Esc, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
            app.picker = None;
            None
        }
        (KeyCode::Enter, _) => {
            // A filter matching nothing leaves no room to select; the modal
            // stays open rather than closing onto an unchanged pane.
            let chosen = app.picker.as_ref()?.selected()?;
            app.picker = None;
            Some(Action::Switch(chosen))
        }
        _ => {
            let picker = app.picker.as_mut()?;
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.cursor = picker.cursor.saturating_sub(1),
                (KeyCode::Down, _) => {
                    picker.cursor =
                        (picker.cursor + 1).min(picker.matches().len().saturating_sub(1))
                }
                (KeyCode::Backspace, _) => {
                    picker.filter.pop();
                    picker.cursor = 0;
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    picker.filter.push(c);
                    // Widening or narrowing changes which room each index
                    // names, so holding the old index would move the cursor
                    // to a room nobody pointed it at.
                    picker.cursor = 0;
                }
                _ => {}
            }
            None
        }
    }
}

/// Decide whether the in-memory tail cursor is stale relative to the log
/// file's own last id. If the file's last id is lower than our cursor, the
/// log was truncated or replaced (e.g. a test fixture reset, or a future
/// "clear room" feature) and ids restarted from 1; tailing from the old
/// cursor would filter out every subsequent message forever. In that case
/// the caller should discard its in-memory messages and re-seed from the
/// start of the file.
///
/// Only ever about the room currently being tailed. A room switch re-seeds
/// unconditionally and must not come through here, which would compare one
/// room's cursor against another room's last id.
fn should_reseed(mem_last_id: u64, file_last_id: u64) -> bool {
    file_last_id < mem_last_id
}

/// Display width of `s` in terminal cells, not chars: a CJK or emoji
/// character occupies two cells but counts as one `char`.
fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Splits `text` into rows of at most `width` display cells, with only
/// `first_width` available on the first row (the `from: ` prefix shares it).
/// Breaks on spaces where it can and hard-breaks a word too long for a row.
/// Always returns at least one row.
///
/// Measures display cells via `unicode_width`, not `char`s: a CJK or emoji
/// character occupies two cells, so counting chars would let such a row
/// overflow the pane and be truncated by the renderer with no way to scroll
/// to the lost half.
fn wrap_text(text: &str, first_width: usize, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut budget = first_width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;

    for word in text.split(' ') {
        let mut word = word;
        loop {
            let wlen = display_width(word);
            let sep = usize::from(cur_len > 0);
            if cur_len + sep + wlen <= budget {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(word);
                cur_len += sep + wlen;
                break;
            }
            if cur_len > 0 {
                // retry this word on a fresh row
                rows.push(std::mem::take(&mut cur));
                cur_len = 0;
                budget = width;
                continue;
            }
            // a single word longer than a whole row: hard-break it, or we
            // would loop forever (reachable at narrow pane widths)
            let mut head = String::new();
            let mut head_width = 0usize;
            let mut consumed = 0usize;
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if head_width + w > budget && !head.is_empty() {
                    break;
                }
                head.push(c);
                head_width += w;
                consumed += c.len_utf8();
                if head_width >= budget {
                    break;
                }
            }
            word = &word[consumed..];
            rows.push(head);
            budget = width;
        }
    }
    rows.push(cur);
    rows
}

/// Renders one message as the rows it actually occupies at `width` columns.
/// `Paragraph::scroll` applies after wrapping, so the scroll arithmetic has
/// to count rendered rows, not messages.
fn message_rows(m: &Message, width: usize) -> Vec<Line<'static>> {
    let who_style = if m.from == "human" {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    let prefix = format!("{}: ", m.from);
    let prefix_len = display_width(&prefix);
    let mut rows = wrap_text(&m.text, width.saturating_sub(prefix_len), width).into_iter();
    let first = rows.next().unwrap_or_default();
    std::iter::once(Line::from(vec![
        Span::styled(prefix, who_style),
        Span::raw(first),
    ]))
    .chain(rows.map(|r| Line::from(Span::raw(r))))
    .collect()
}

/// Index of the first row to render. `scroll_from_bottom` is clamped to the
/// bottom offset so the newest row is always reachable.
fn scroll_start(total_rows: usize, visible_rows: usize, scroll_from_bottom: usize) -> usize {
    let bottom = total_rows.saturating_sub(visible_rows);
    bottom.saturating_sub(scroll_from_bottom.min(bottom))
}

/// Message-pane title. Always names the room being viewed — a group, the
/// ungrouped room, or none selected — so no state can be on screen
/// unlabelled.
///
/// The name comes from `CurrentRoom::label`, the same spelling the picker
/// rows show and its filter searches, so what the title says is what you
/// can type to get back here. Spelling `(ungrouped)` a second time is how
/// the two would drift apart. `label` is parenthesised for exactly this:
/// `valid_group_name` forbids parentheses, so it can never collide with a
/// real group's name.
///
/// It is only half the safeguard against typing into the wrong company's
/// room now that a pane can switch: this titles the *messages*, while the
/// input line — where a post is committed — carries its own marker once the
/// pane is away from the room it opened in.
fn title_for(room: &CurrentRoom) -> String {
    format!(" scuttlebutt · {} ", room.label())
}

/// The members pane's roster, scoped to the resolved group. The initial
/// seed, the periodic refresh and every room switch go through here: the
/// pane sits beside a title naming the group, so an unscoped roster would
/// show one company's agent names in another company's room. `None` on a
/// failed listing, so the refresh can keep the last known roster instead of
/// blanking the pane for a transient `herdr agent list` failure.
///
/// `resolved` is a group or the ungrouped room, never "no room selected":
/// `None` here means the shared room's roster, which is a real answer and
/// the wrong one for a pane that has not picked a room.
fn scoped_members(
    herd: &dyn HerdControl,
    resolved: Option<&str>,
    grouping: &crate::groups::Grouping,
    orgs: &mut crate::git_org::OrgCache,
) -> Option<Vec<AgentInfo>> {
    let all = herd.list_agents().ok()?;
    Some(
        crate::cli::visible_agents(&all, resolved, grouping, orgs)
            .into_iter()
            .cloned()
            .collect(),
    )
}

/// A room's directory under `session_dir`, or `None` when no room is
/// selected. Shares `paths::room_dir_in` with `room_dir` rather than joining
/// the path a second time, and inherits its refusal to create anything: this
/// runs for every room on every picker open.
fn room_path(session_dir: &Path, room: &CurrentRoom) -> Option<PathBuf> {
    room.selected()
        .map(|g| crate::paths::room_dir_in(session_dir, g))
}

/// Bytes in a room's `room.jsonl`, 0 if it has none.
///
/// Length, never `log_store::last_id`: that answers the same question by
/// parsing the whole file — 292 KB for one real room — and the unread check
/// runs for every room each time the picker opens.
fn room_len(session_dir: &Path, room: &CurrentRoom) -> u64 {
    room_path(session_dir, room)
        .and_then(|d| std::fs::metadata(d.join("room.jsonl")).ok())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Every room's log length at pane start, which is what an unread dot is
/// measured against. Swept straight off disk: this runs before the first
/// draw, and `groups::rooms` would spend a `herdr agent list` subprocess to
/// answer a question about file sizes.
///
/// A room created after this sweep has no seed and reads as 0, so it is
/// dotted — correct, since everything in it did arrive while the pane was
/// open.
fn seed_unread(session_dir: &Path) -> HashMap<CurrentRoom, u64> {
    let mut seeds = HashMap::new();
    seeds.insert(
        CurrentRoom::Ungrouped,
        room_len(session_dir, &CurrentRoom::Ungrouped),
    );
    if let Ok(entries) = std::fs::read_dir(session_dir) {
        for e in entries.flatten() {
            if let Ok(name) = e.file_name().into_string() {
                let room = CurrentRoom::Named(name);
                let len = room_len(session_dir, &room);
                seeds.insert(room, len);
            }
        }
    }
    seeds
}

/// Builds the modal's rows. Called on every open so the list — live agent
/// counts included — is current at the moment someone reads it.
fn open_picker(
    app: &App,
    session_dir: &Path,
    herd: &dyn HerdControl,
    grouping: &Grouping,
    orgs: &mut crate::git_org::OrgCache,
) -> PickerState {
    let agents = herd.list_agents().unwrap_or_default();
    let rooms: Vec<Room> = crate::groups::rooms(grouping, &agents, session_dir, orgs);
    let rows = rooms
        .iter()
        .map(|r| {
            let room = CurrentRoom::from(r);
            PickerRow {
                agents: r.agents,
                // The one derivation of provenance, shared with the order
                // `rooms` already sorted these into.
                sources: r
                    .sources()
                    .iter()
                    .map(|s| s.label())
                    .collect::<Vec<_>>()
                    .join(", "),
                // Changed, not merely grown: a truncated or replaced log
                // is shorter than its seed, and `>` would leave it
                // permanently undotted however much arrived afterwards —
                // the case `should_reseed` exists for, one file along.
                unread: room != app.room
                    && room_len(session_dir, &room)
                        != app.unread_seeds.get(&room).copied().unwrap_or(0),
                room,
            }
        })
        .collect();
    PickerState {
        rows,
        filter: String::new(),
        cursor: 0,
    }
}

/// Points the pane at `target`, moving everything that is scoped to a room.
///
/// Members are refreshed here rather than left to the 3-second tick, which
/// would otherwise leave one room's agent names beside another room's title
/// — the leak `scoped_members` exists to prevent. The caller resets its
/// refresh timer for the same reason.
fn switch_room(
    app: &mut App,
    target: CurrentRoom,
    session_dir: &Path,
    herd: &dyn HerdControl,
    grouping: &Grouping,
    orgs: &mut crate::git_org::OrgCache,
) -> Result<()> {
    if target == app.room {
        return Ok(());
    }
    // Cleared before anything that can fail, because the caller writes its
    // own `could not open that room: …` into `last_error` when this returns
    // Err — and a flag left standing there hands that message to the next
    // successful read to erase.
    app.tail_failed = false;
    // Everything that can fail happens here, before a single byte of the
    // pane's room-scoped state moves. `paths::room_dir` reaches `base_dir`,
    // which spawns `herdr plugin config-dir` unless SCUTTLEBUTT_DIR is set,
    // so a failing switch is a subprocess hiccup away rather than
    // theoretical — and a half-applied one leaves room A on screen holding
    // room B's draft, which is the next Enter posting one company's text
    // into another company's room.
    let loaded = match target.selected() {
        Some(group) => {
            let dir = crate::paths::room_dir_in(session_dir, group);
            // A clean read of the whole file, never `should_reseed`: that
            // asks whether *this* room's log was truncated, and it would be
            // comparing this room's cursor against another room's last id.
            // A room with no directory yet reads as empty rather than
            // failing, which is why the read can precede the create below.
            let messages = log_store::read_since(&dir, 0)?;
            // A failed listing is not a failed switch: `scoped_members`
            // returns `None` for it, and an empty roster beside the new
            // room's title is honest, where the old room's names would not
            // be.
            let members = scoped_members(herd, group, grouping, orgs).unwrap_or_default();
            // Created only once the room is certain to open, so a switch
            // that fails leaves no empty directory behind — the litter
            // `groups::has_history` exists to sweep out of listings.
            // `room_dir` would have created it before the read could fail,
            // and reaches `base_dir`, which spawns a subprocess per switch.
            std::fs::create_dir_all(&dir)?;
            Some((dir, messages, members))
        }
        None => None,
    };

    // From here nothing fails, so the pane cannot be left half-switched.
    let carried = std::mem::take(&mut app.input);
    if app.room.selected().is_some() {
        app.drafts.insert(app.room.clone(), carried);
        // Re-seed the room being left so visiting it clears its dot; its log
        // has been read up to here.
        let len = room_len(session_dir, &app.room);
        app.unread_seeds.insert(app.room.clone(), len);
        app.input = app.drafts.remove(&target).unwrap_or_default();
    } else {
        // Text typed with no room selected was not typed *for* a room —
        // there was none — so it follows the human into the first one they
        // pick rather than being stashed under a room nobody can return to.
        // Stashing it would be the silent discard per-room drafts exist to
        // prevent, since `NoneSelected` is unreachable again once a room is
        // chosen.
        app.input = app.drafts.remove(&target).unwrap_or(carried);
    }
    app.scroll_from_bottom = 0;
    // A failure belonged to the room it happened in — a write attempted
    // there, or a read of that room's log.
    app.last_error = None;

    match loaded {
        Some((dir, messages, members)) => {
            app.messages = messages;
            app.members = members;
            app.dir = Some(dir);
        }
        None => {
            // `scoped_members(None, ..)` is the *ungrouped* room's roster,
            // so it is not the answer here: no room is selected, and nobody
            // is in one.
            app.messages = Vec::new();
            app.members = Vec::new();
            app.dir = None;
        }
    }
    app.title = title_for(&target);
    app.room = target;
    Ok(())
}

/// Commits a line to the room on screen, reporting a failed write the way a
/// failed read and a failed switch are reported.
fn post(app: &mut App, dir: &Path, text: &str) {
    app.last_error = match log_store::append(dir, "human", text) {
        Ok(_) => None,
        Err(e) => Some(format!("post failed: {e}")),
    };
    // Whatever is in `last_error` now, it is not the tail's. Leaving the
    // flag standing would have the next good read — which comes before the
    // next draw — erase what the human just failed to send, unseen.
    app.tail_failed = false;
}

/// The messages `dir` has for a pane sitting at `cursor`, and whether they
/// replace that pane's list rather than extend it: `room.jsonl` was
/// truncated or replaced (ids restarted lower than the cursor), so the stale
/// in-memory tail is discarded and the file re-read from the start rather
/// than every future message being filtered out forever.
///
/// Separate from `tail` so that everything which can fail happens before a
/// single message moves — the same order `switch_room` uses, and what lets a
/// failed read leave the list exactly as it stands.
///
/// An absent file is silent only before this pane has read any history. Once
/// `cursor` is non-zero, absence is a failed read rather than an empty log:
/// the latter exists and deliberately re-seeds to an empty pane, while the
/// former preserves the visible messages until a rotation finishes putting
/// the file back. Reading presence and contents in one operation avoids a
/// deletion between a separate existence check and the read.
fn read_tail(dir: &Path, cursor: u64) -> Result<(bool, Vec<Message>)> {
    let Some(messages) = log_store::read_since_if_present(dir, 0)? else {
        if cursor == 0 {
            return Ok((false, Vec::new()));
        }
        anyhow::bail!("room log is missing");
    };
    let file_last_id = messages.last().map(|m| m.id).unwrap_or(0);
    let reseed = should_reseed(cursor, file_last_id);
    let from = if reseed { 0 } else { cursor };
    Ok((
        reseed,
        messages.into_iter().filter(|m| m.id > from).collect(),
    ))
}

/// Reads whatever has arrived in the room on screen since the last loop,
/// surfacing a failed read through `last_error` rather than out of the event
/// loop. Propagating it tears the pane down, and `App` goes with it — every
/// room's stashed draft included, silently.
///
/// A failure does not stop this room being polled. The read is one file read
/// per loop; leaving it running is what makes a genuinely unreadable room
/// keep saying so on every frame instead of complaining once and then
/// looking healthy, and it is also the only way a room that becomes readable
/// again recovers, which a poller that had switched itself off could not do.
fn tail(app: &mut App, dir: &Path) {
    let cursor = app.messages.last().map(|m| m.id).unwrap_or(0);
    match read_tail(dir, cursor) {
        Ok((reseed, mut fresh)) => {
            if reseed {
                app.messages = fresh;
            } else {
                app.messages.append(&mut fresh);
            }
            // Only the read's own error is cleared, never whatever else is
            // sitting in `last_error`: this runs at the top of every loop,
            // before the draw, so clearing unconditionally would wipe a
            // "post failed: …" from the keystroke just handled before the
            // human ever saw it.
            if std::mem::take(&mut app.tail_failed) {
                app.last_error = None;
            }
        }
        Err(e) => {
            app.last_error = Some(format!("could not read this room: {e}"));
            app.tail_failed = true;
        }
    }
}

pub fn run(group: Option<&str>) -> Result<()> {
    let grouping = crate::groups::load(&crate::paths::base_dir()?);
    let mut orgs = crate::git_org::OrgCache::default();
    // Not `resolve_group`: an active config matching nothing opens the pane
    // with no room selected and the picker up, which is the one state where
    // a switcher is the whole point. `Broken` still bails inside.
    let home = crate::cli::resolve_room(group, &std::env::current_dir()?, &grouping, &mut orgs)?;
    let session_dir = crate::paths::session_dir()?;
    let herd = RealHerd;
    let mut app = App {
        unread_seeds: seed_unread(&session_dir),
        // Seeded by the switch below, which is the same code path every
        // later switch takes.
        title: title_for(&CurrentRoom::NoneSelected),
        home: home.clone(),
        ..App::default()
    };
    switch_room(&mut app, home, &session_dir, &herd, &grouping, &mut orgs)?;
    if app.room.selected().is_none() {
        app.picker = Some(open_picker(&app, &session_dir, &herd, &grouping, &mut orgs));
    }

    let mut terminal = ratatui::init();
    let mut last_member_refresh = std::time::Instant::now();
    let result = (|| -> Result<()> {
        while !app.quit {
            // tail new messages every loop; members on a slow tick
            if let Some(dir) = app.dir.clone() {
                tail(&mut app, &dir);
                // `selected()` is destructured, never flattened: flattening
                // would hand `scoped_members` the ungrouped room's roster
                // for a pane that has selected no room, which is the
                // conflation `CurrentRoom` exists to make unsayable. The
                // outer `if let` makes that unreachable today; relying on
                // that would leave the leak one refactor away.
                if let Some(group) = app.room.selected().map(|g| g.map(str::to_string)) {
                    if last_member_refresh.elapsed() > std::time::Duration::from_secs(3) {
                        if let Some(m) =
                            scoped_members(&herd, group.as_deref(), &grouping, &mut orgs)
                        {
                            app.members = m;
                        }
                        last_member_refresh = std::time::Instant::now();
                    }
                }
            }

            terminal.draw(|f| draw(f, &app))?;

            if event::poll(std::time::Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match handle_key(&mut app, key.code, key.modifiers) {
                            Some(Action::Post(text)) => {
                                // `handle_key` only returns this with a room
                                // selected, so a pane with none never reaches
                                // an append with no directory.
                                if let Some(dir) = app.dir.clone() {
                                    post(&mut app, &dir, &text);
                                }
                            }
                            Some(Action::OpenPicker) => {
                                app.picker = Some(open_picker(
                                    &app,
                                    &session_dir,
                                    &herd,
                                    &grouping,
                                    &mut orgs,
                                ));
                            }
                            Some(Action::Switch(target)) => {
                                match switch_room(
                                    &mut app,
                                    target,
                                    &session_dir,
                                    &herd,
                                    &grouping,
                                    &mut orgs,
                                ) {
                                    // `switch_room` just refreshed the
                                    // roster; without this the tick could
                                    // fire again immediately and is wasted
                                    // work, and a failed listing would blank
                                    // the pane it just filled.
                                    Ok(()) => last_member_refresh = std::time::Instant::now(),
                                    // Same standard the input line already
                                    // holds write failures to: a transient
                                    // failure must not tear the chat pane
                                    // down. `switch_room` is a no-op when it
                                    // fails, so the pane is still showing
                                    // the room it was in, with every draft
                                    // where it was left.
                                    Err(e) => {
                                        app.last_error =
                                            Some(format!("could not open that room: {e}"))
                                    }
                                }
                            }
                            None => {}
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(f.area());
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(24)])
        .split(outer[0]);

    // Pre-wrap to the pane's inner width and scroll by rendered row. Letting
    // Paragraph wrap instead would make one long message count as a single
    // row here while occupying several on screen, pushing the newest
    // messages below the viewport where Down cannot reach them.
    let inner_width = top[0].width.saturating_sub(2) as usize;
    let rows: Vec<Line> = app
        .messages
        .iter()
        .flat_map(|m| message_rows(m, inner_width))
        .collect();
    let visible = top[0].height.saturating_sub(2) as usize;
    let start = scroll_start(rows.len(), visible, app.scroll_from_bottom as usize);
    f.render_widget(
        Paragraph::new(rows[start.min(rows.len())..].to_vec()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.title.as_str()),
        ),
        top[0],
    );

    let members: Vec<ListItem> = app
        .members
        .iter()
        .map(|a| {
            let color = match a.status.as_str() {
                "idle" | "done" => Color::Green,
                "working" => Color::Blue,
                "blocked" => Color::Red,
                _ => Color::DarkGray,
            };
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::default().fg(color)),
                Span::raw(a.name.clone()),
            ]))
        })
        .chain(std::iter::once(ListItem::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(Color::Yellow)),
            Span::raw("human (you)"),
        ]))))
        .collect();
    f.render_widget(
        List::new(members).block(Block::default().borders(Borders::ALL).title(" members ")),
        top[1],
    );

    // The input line is where a post is committed, so it carries the
    // away-from-home marker: a colour *and* the room's name, because a
    // colour alone says nothing about which room you are about to post in.
    let away = app.room.selected().is_some() && app.room != app.home;
    // A post failure never displaces the away-from-home marker. The failure
    // is the moment the human is about to retype and press Enter, so
    // swapping the room name out for the error would switch the safeguard
    // off at the one moment it is doing work. Both are shown, room first,
    // and the border stays away-yellow rather than error-red: a reader who
    // has learned that yellow means "not your room" must not lose it to a
    // transient write error.
    let (input_title, border) = match (&app.last_error, app.room.selected().is_some(), away) {
        // A pane with no room selected keeps its one affordance whatever
        // else has failed. Ctrl-K is the only way out of a state the pane
        // cannot leave on its own — the state #35 exists to rescue — so an
        // error that displaced the hint would make the pane dead again.
        (Some(e), false, _) => (format!(" {e} — Ctrl-K to pick a room "), Color::Red),
        (Some(e), _, true) => (format!(" → {} · {e} ", app.room.label()), Color::Yellow),
        (Some(e), _, false) => (format!(" {e} "), Color::Red),
        (None, false, _) => (
            " no room selected — Ctrl-K to pick a room ".to_string(),
            Color::DarkGray,
        ),
        (None, true, true) => (
            format!(
                " message → {} (Enter to send, Ctrl-K to switch) ",
                app.room.label()
            ),
            Color::Yellow,
        ),
        (None, true, false) => (
            " message (Enter to send, Ctrl-K rooms, Esc to quit) ".to_string(),
            Color::Reset,
        ),
    };
    f.render_widget(
        Paragraph::new(app.input.as_str()).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border))
                .title(input_title),
        ),
        outer[1],
    );

    if let Some(picker) = &app.picker {
        draw_picker(f, picker);
    }
}

/// The modal room list, drawn last and over a cleared rectangle so the chat
/// behind it cannot bleed through and be mistaken for a row.
fn draw_picker(f: &mut ratatui::Frame, picker: &PickerState) {
    let area = f.area();
    // `.min(area.*)` is not redundant with the subtraction: a pane only a
    // few cells tall subtracts to 0 and the lower clamp bound pushes it back
    // above the buffer.
    let w = area.width.saturating_sub(8).clamp(1, 64).min(area.width);
    let h = area.height.saturating_sub(4).clamp(3, 16).min(area.height);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let matches = picker.matches();
    let items: Vec<ListItem> = matches
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let row = if i == picker.cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(
                Line::from(vec![
                    Span::styled(
                        if r.unread { "● " } else { "  " },
                        Style::default().fg(Color::Yellow),
                    ),
                    // Padded by display cells, not chars, like every other
                    // width in this file: a CJK room name is twice as wide
                    // as it is long and would push the column out of line.
                    Span::styled(
                        format!(
                            "{}{}",
                            r.room.label(),
                            " ".repeat(20usize.saturating_sub(display_width(r.room.label())))
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{} agents · {}", r.agents, r.sources)),
                ])
                .style(row),
            )
        })
        .collect();
    let title = format!(" rooms · filter: {}_ ", picker.filter);
    f.render_widget(Clear, rect);
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(title),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// A pane in the ungrouped room. Explicit because `App::default()` is a
    /// pane with *no* room selected, where posting is disabled by design.
    fn app() -> App {
        App {
            room: CurrentRoom::Ungrouped,
            home: CurrentRoom::Ungrouped,
            ..App::default()
        }
    }

    #[test]
    fn typing_appends_to_input() {
        let mut a = app();
        handle_key(&mut a, KeyCode::Char('h'), KeyModifiers::NONE);
        handle_key(&mut a, KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(a.input, "hi");
    }

    #[test]
    fn enter_submits_and_clears() {
        let mut a = app();
        a.input = "hello".into();
        let submitted = handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(submitted, Some(Action::Post("hello".into())));
        assert_eq!(a.input, "");
    }

    #[test]
    fn enter_on_empty_input_is_noop() {
        let mut a = app();
        assert_eq!(handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE), None);
    }

    #[test]
    fn enter_on_whitespace_only_input_is_noop() {
        let mut a = app();
        a.input = "   ".into();
        assert_eq!(handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE), None);
    }

    #[test]
    fn ctrl_c_and_esc_quit() {
        let mut a = app();
        handle_key(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.quit);
        let mut b = app();
        handle_key(&mut b, KeyCode::Esc, KeyModifiers::NONE);
        assert!(b.quit);
    }

    #[test]
    fn should_reseed_when_file_last_id_is_lower_than_cursor() {
        // room.jsonl was truncated/replaced and ids restarted lower.
        assert!(should_reseed(5, 2));
    }

    #[test]
    fn should_not_reseed_on_normal_growth() {
        assert!(!should_reseed(2, 5));
        assert!(!should_reseed(2, 2));
        assert!(!should_reseed(0, 0));
    }

    fn msg(from: &str, text: &str) -> Message {
        Message {
            id: 1,
            ts: "t".into(),
            from: from.into(),
            text: text.into(),
        }
    }

    #[test]
    fn short_message_is_one_row() {
        assert_eq!(message_rows(&msg("bob", "hi"), 40).len(), 1);
    }

    #[test]
    fn long_message_occupies_several_rows() {
        // "bob: " is 5 columns, leaving 15 on the first row of a 20-wide pane.
        let m = msg("bob", "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj");
        let rows = message_rows(&m, 20);
        assert!(
            rows.len() >= 3,
            "expected multiple rows, got {}",
            rows.len()
        );
        for (i, row) in rows.iter().enumerate() {
            let w: usize = row.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 20, "row {i} is {w} wide: {row:?}");
        }
    }

    #[test]
    fn wide_sender_name_prefix_is_measured_in_display_cells() {
        // "田中" is 2 chars but 4 display cells; the "田中: " prefix must
        // consume its true width from the first-row budget, or the row
        // overflows the pane's inner width.
        let m = msg("田中", "abcdef");
        let rows = message_rows(&m, 10);
        let first_row_width: usize = rows[0]
            .spans
            .iter()
            .map(|s| display_width(&s.content))
            .sum();
        assert!(
            first_row_width <= 10,
            "first row is {first_row_width} cells wide: {:?}",
            rows[0]
        );
    }

    struct FakeHerd(Vec<AgentInfo>, bool);
    impl HerdControl for FakeHerd {
        fn list_agents(&self) -> Result<Vec<AgentInfo>> {
            if self.1 {
                anyhow::bail!("herdr is down");
            }
            Ok(self.0.clone())
        }
        fn prompt(&self, _: &str, _: &str) -> Result<crate::herd::Delivery> {
            Ok(crate::herd::Delivery::Submitted)
        }
    }

    fn at(name: &str, cwd: &str) -> AgentInfo {
        AgentInfo {
            name: name.into(),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
            cwd: cwd.into(),
            focused: Some(false),
            session: None,
        }
    }

    fn two_groups() -> crate::groups::Grouping {
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

    fn no_org(_cwd: &std::path::Path) -> Option<String> {
        None
    }

    /// `/w/<org>/...` belongs to `<org>`; anything else is outside a repo.
    fn fake_org(cwd: &std::path::Path) -> Option<String> {
        let s = cwd.to_string_lossy();
        let rest = s.strip_prefix("/w/")?;
        Some(rest.split('/').next()?.to_string())
    }

    fn orgs(lookup: fn(&std::path::Path) -> Option<String>) -> crate::git_org::OrgCache {
        crate::git_org::OrgCache::with_lookup(lookup, std::time::Duration::from_secs(300))
    }

    #[test]
    fn members_pane_shows_only_the_callers_group() {
        let herd = FakeHerd(
            vec![at("issue-590", "/w/alare/api"), at("acme-x", "/w/acme")],
            false,
        );
        let members =
            scoped_members(&herd, Some("alare"), &two_groups(), &mut orgs(no_org)).unwrap();
        let names: Vec<&str> = members.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["issue-590"]);
    }

    #[test]
    fn members_pane_is_scoped_to_the_org_room_without_a_config() {
        let herd = FakeHerd(
            vec![at("issue-590", "/w/alare/api"), at("acme-x", "/w/acme")],
            false,
        );
        let members = scoped_members(
            &herd,
            Some("alare"),
            &crate::groups::Grouping::Inactive,
            &mut orgs(fake_org),
        )
        .unwrap();
        let names: Vec<&str> = members.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["issue-590"]);
    }

    #[test]
    fn members_pane_shows_the_shared_room_for_agents_outside_a_repo() {
        let herd = FakeHerd(
            vec![at("issue-590", "/w/alare/api"), at("acme-x", "/w/acme")],
            false,
        );
        let members = scoped_members(
            &herd,
            None,
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        )
        .unwrap();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn failed_listing_leaves_the_roster_untouched() {
        // a transient `herdr agent list` failure must not blank the pane
        let herd = FakeHerd(vec![], true);
        assert!(scoped_members(&herd, Some("alare"), &two_groups(), &mut orgs(no_org)).is_none());
    }

    #[test]
    fn title_for_names_the_group() {
        assert!(title_for(&named("alare")).contains("alare"));
    }

    #[test]
    fn title_for_names_the_ungrouped_room() {
        // Every room is named in the title, the ungrouped one included; a
        // bare " scuttlebutt " left the one room with no label on screen.
        assert_eq!(
            title_for(&CurrentRoom::Ungrouped),
            " scuttlebutt · (ungrouped) "
        );
    }

    #[test]
    fn title_for_names_a_pane_that_has_no_room() {
        assert_eq!(
            title_for(&CurrentRoom::NoneSelected),
            " scuttlebutt · no room selected "
        );
    }

    #[test]
    fn the_title_spells_a_room_the_way_the_picker_filter_reads_it() {
        // `label`'s promise is "what you can see is what you can type", and
        // a title that spelled the room a second way would break it without
        // failing any test of `label` alone.
        for room in [
            named("alare"),
            CurrentRoom::Ungrouped,
            CurrentRoom::NoneSelected,
        ] {
            assert!(
                title_for(&room).contains(room.label()),
                "title {:?} does not contain {:?}",
                title_for(&room),
                room.label()
            );
        }
    }

    #[test]
    fn the_ungrouped_label_can_never_collide_with_a_real_group() {
        assert!(!crate::groups::valid_group_name(
            CurrentRoom::Ungrouped.label()
        ));
    }

    /// Renders `draw` into an off-screen terminal and returns the rows as
    /// strings, so a test can assert what a user would actually see.
    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Terminal column of each row containing `needle`, measured in cells
    /// rather than string bytes. `render` above concatenates cell symbols,
    /// and a wide glyph fills one cell and leaves the next holding a space,
    /// so an index into that string is not a column: two aligned rows can
    /// look misaligned there, and misaligned ones aligned.
    fn columns_of(app: &App, width: u16, height: u16, needle: &str) -> Vec<u16> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut hits = Vec::new();
        for y in 0..height {
            let cells: Vec<(u16, String)> = (0..width)
                .map(|x| (x, buffer[(x, y)].symbol().to_string()))
                .collect();
            let line: String = cells.iter().map(|(_, s)| s.as_str()).collect();
            if let Some(byte) = line.find(needle) {
                let mut acc = 0usize;
                for (x, s) in &cells {
                    if acc == byte {
                        hits.push(*x);
                        break;
                    }
                    acc += s.len();
                }
            }
        }
        hits
    }

    #[test]
    fn newest_message_is_visible_without_scrolling() {
        // The bug: counting one row per message undershoots the bottom
        // offset, so the tail of a wrapped message renders below the
        // viewport and Down saturates before it can be reached. Asserted
        // against the rendered buffer, because scroll arithmetic alone is
        // self-consistent whether or not it agrees with the renderer.
        let mut app = App {
            messages: (0..6)
                .map(|i| {
                    msg(
                        "bob",
                        &format!("filler message number {i} padded out a bit"),
                    )
                })
                .collect(),
            ..App::default()
        };
        app.messages
            .push(msg("bob", "one two three four five six seven NEWEST"));

        let screen = render(&app, 60, 12).join("\n");
        assert!(
            screen.contains("NEWEST"),
            "newest message is off-screen:\n{screen}"
        );
    }

    #[test]
    fn scrolling_up_then_back_down_returns_to_the_newest_message() {
        let mut app = App {
            messages: (0..8)
                .map(|i| {
                    msg(
                        "bob",
                        &format!("filler message number {i} padded out a bit"),
                    )
                })
                .collect(),
            ..App::default()
        };
        app.messages
            .push(msg("bob", "one two three four five six seven NEWEST"));

        for _ in 0..3 {
            handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        }
        assert!(!render(&app, 60, 12).join("\n").contains("NEWEST"));
        for _ in 0..3 {
            handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert!(render(&app, 60, 12).join("\n").contains("NEWEST"));
    }

    #[test]
    fn last_error_is_surfaced_in_the_input_title() {
        let app = App {
            last_error: Some("post failed: disk on fire".into()),
            ..App::default()
        };
        assert!(render(&app, 60, 12).join("\n").contains("disk on fire"));
    }

    #[test]
    fn scroll_start_clamps_to_the_top_and_bottom() {
        assert_eq!(scroll_start(10, 3, 0), 7);
        assert_eq!(scroll_start(10, 3, 2), 5);
        // scrolling further up than there is history stops at the first row
        assert_eq!(scroll_start(10, 3, 999), 0);
        // everything fits: no scrolling at all
        assert_eq!(scroll_start(2, 10, 0), 0);
        assert_eq!(scroll_start(2, 10, 5), 0);
        assert_eq!(scroll_start(0, 0, 0), 0);
    }

    #[test]
    fn wrap_text_hard_breaks_an_overlong_word() {
        let rows = wrap_text("abcdefghij", 4, 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_text_survives_zero_width() {
        // A 1-column pane leaves no room after the prefix; this must
        // terminate rather than spin.
        let rows = wrap_text("hello world", 0, 0);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.chars().count() <= 1));
    }

    #[test]
    fn wrap_text_of_empty_string_is_one_row() {
        assert_eq!(wrap_text("", 10, 10), vec![""]);
    }

    #[test]
    fn wraps_on_display_width_not_char_count() {
        // each CJK char is two cells wide, so four of them fill an 8-cell row
        let rows = wrap_text("一二三四五六", 8, 8);
        assert_eq!(rows, vec!["一二三四".to_string(), "五六".to_string()]);
    }

    #[test]
    fn wide_text_is_never_truncated() {
        let text = "一二三四五六七八九十";
        let rows = wrap_text(text, 8, 8);
        let rejoined: String = rows.concat();
        assert_eq!(rejoined, text);
    }

    // --- picker ---

    fn picker_of(names: &[&str]) -> PickerState {
        PickerState {
            rows: names
                .iter()
                .map(|n| PickerRow {
                    room: CurrentRoom::Named((*n).into()),
                    agents: 0,
                    sources: "config".into(),
                    unread: false,
                })
                .collect(),
            filter: String::new(),
            cursor: 0,
        }
    }

    #[test]
    fn ctrl_k_asks_to_open_the_picker() {
        let mut a = app();
        assert_eq!(
            handle_key(&mut a, KeyCode::Char('k'), KeyModifiers::CONTROL),
            Some(Action::OpenPicker)
        );
    }

    #[test]
    fn esc_closes_the_picker_instead_of_quitting_the_pane() {
        // An unconditional quit here would train people to lose the pane.
        let mut a = app();
        a.picker = Some(picker_of(&["alare"]));
        handle_key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
        assert!(a.picker.is_none());
        assert!(!a.quit);
        // and only then does Esc quit
        handle_key(&mut a, KeyCode::Esc, KeyModifiers::NONE);
        assert!(a.quit);
    }

    #[test]
    fn typing_behind_an_open_picker_never_reaches_the_message_input() {
        let mut a = app();
        a.picker = Some(picker_of(&["alare"]));
        handle_key(&mut a, KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(a.input, "");
        assert_eq!(a.picker.as_ref().unwrap().filter, "h");
    }

    #[test]
    fn the_filter_matches_a_substring_not_a_prefix() {
        // Group names here share stems and hyphens, so prefix matching would
        // make the longest names the hardest to reach.
        let mut a = app();
        a.picker = Some(picker_of(&["herdr-scuttlebutt", "alare"]));
        for c in "scuttle".chars() {
            handle_key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
        }
        let p = a.picker.as_ref().unwrap();
        let shown: Vec<&str> = p.matches().iter().map(|r| r.room.label()).collect();
        assert_eq!(shown, vec!["herdr-scuttlebutt"]);
    }

    #[test]
    fn the_filter_ignores_case_in_both_directions() {
        let mut a = app();
        a.picker = Some(picker_of(&["Alare"]));
        for c in "aLaR".chars() {
            handle_key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(a.picker.as_ref().unwrap().matches().len(), 1);
    }

    #[test]
    fn enter_selects_the_room_under_the_cursor() {
        let mut a = app();
        a.picker = Some(picker_of(&["acme", "alare"]));
        handle_key(&mut a, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::Switch(CurrentRoom::Named("alare".into())))
        );
        assert!(a.picker.is_none());
    }

    #[test]
    fn the_cursor_stops_at_both_ends_of_the_list() {
        let mut a = app();
        a.picker = Some(picker_of(&["acme", "alare"]));
        for _ in 0..5 {
            handle_key(&mut a, KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(a.picker.as_ref().unwrap().cursor, 1);
        for _ in 0..5 {
            handle_key(&mut a, KeyCode::Up, KeyModifiers::NONE);
        }
        assert_eq!(a.picker.as_ref().unwrap().cursor, 0);
    }

    #[test]
    fn narrowing_the_filter_cannot_leave_the_cursor_on_a_room_nobody_chose() {
        // The bug this prevents: cursor on row 2, type until only row 0
        // survives, press Enter, and switch to whatever the stale index now
        // names — or index past the list entirely.
        let mut a = app();
        a.picker = Some(picker_of(&["acme", "alare", "zebra"]));
        handle_key(&mut a, KeyCode::Down, KeyModifiers::NONE);
        handle_key(&mut a, KeyCode::Down, KeyModifiers::NONE);
        handle_key(&mut a, KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(
            handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::Switch(CurrentRoom::Named("acme".into())))
        );
    }

    #[test]
    fn enter_on_a_filter_that_matches_nothing_leaves_the_picker_open() {
        let mut a = app();
        a.picker = Some(picker_of(&["alare"]));
        for c in "zzz".chars() {
            handle_key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(a.picker.is_some());
        assert!(!a.quit);
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut a = app();
        a.picker = Some(picker_of(&["alare", "acme"]));
        for c in "al".chars() {
            handle_key(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(a.picker.as_ref().unwrap().matches().len(), 1);
        handle_key(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(a.picker.as_ref().unwrap().matches().len(), 2);
    }

    #[test]
    fn ctrl_c_still_quits_from_inside_the_picker() {
        let mut a = app();
        a.picker = Some(picker_of(&["alare"]));
        handle_key(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.quit);
    }

    // --- no room selected ---

    #[test]
    fn a_pane_with_no_room_selected_cannot_post() {
        // Not merely "nothing is written": the draft must survive, because
        // clearing the input would discard it with nothing written anywhere.
        let mut a = App {
            input: "hello".into(),
            ..App::default()
        };
        assert_eq!(handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(a.input, "hello");
    }

    #[test]
    fn a_pane_with_no_room_selected_says_so_and_offers_the_picker() {
        let a = App {
            title: " scuttlebutt · no room selected ".into(),
            ..App::default()
        };
        let screen = render(&a, 60, 12).join("\n");
        assert!(screen.contains("no room selected"), "{screen}");
        assert!(screen.contains("Ctrl-K"), "{screen}");
    }

    #[test]
    fn away_from_home_the_input_line_names_the_room_it_would_post_to() {
        let a = App {
            room: CurrentRoom::Named("acme".into()),
            home: CurrentRoom::Named("alare".into()),
            ..App::default()
        };
        let screen = render(&a, 60, 12).join("\n");
        assert!(screen.contains("acme"), "{screen}");
    }

    #[test]
    fn at_home_the_input_line_carries_no_room_marker() {
        let a = App {
            room: CurrentRoom::Named("alare".into()),
            home: CurrentRoom::Named("alare".into()),
            ..App::default()
        };
        assert!(!render(&a, 60, 12).join("\n").contains("→"));
    }

    #[test]
    fn the_picker_renders_over_the_chat_behind_it() {
        let mut a = app();
        a.messages = (0..8)
            .map(|i| msg("bob", &format!("chatter number {i} in the room")))
            .collect();
        a.picker = Some(PickerState {
            rows: vec![PickerRow {
                room: CurrentRoom::Named("alare".into()),
                agents: 2,
                sources: "live agents, config".into(),
                unread: true,
            }],
            filter: String::new(),
            cursor: 0,
        });
        let screen = render(&a, 90, 14).join("\n");
        assert!(screen.contains("alare"), "{screen}");
        assert!(screen.contains("2 agents"), "{screen}");
        assert!(screen.contains("live agents, config"), "{screen}");
        assert!(screen.contains("●"), "no unread dot:\n{screen}");
    }

    // --- switch mechanics ---

    /// A scratch session dir wired up as the process's `SCUTTLEBUTT_DIR`, so
    /// `switch_room` resolves rooms through `paths::room_dir` exactly as it
    /// does in a real pane.
    fn scratch() -> (
        tempfile::TempDir,
        std::sync::MutexGuard<'static, ()>,
        PathBuf,
    ) {
        let env = crate::paths::env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SCUTTLEBUTT_DIR", dir.path());
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/switch-test.sock");
        let session = crate::paths::session_dir().unwrap();
        (dir, env, session)
    }

    fn switch(
        app: &mut App,
        target: CurrentRoom,
        session: &Path,
        herd: &dyn HerdControl,
    ) -> Result<()> {
        switch_room(
            app,
            target,
            session,
            herd,
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        )
    }

    fn named(n: &str) -> CurrentRoom {
        CurrentRoom::Named(n.into())
    }

    #[test]
    fn switching_shows_the_target_rooms_messages_and_title() {
        let (_d, _env, session) = scratch();
        log_store::append(
            &crate::paths::room_dir(Some("acme")).unwrap(),
            "bob",
            "in acme",
        )
        .unwrap();
        let mut a = app();
        a.messages = vec![msg("bob", "in the old room")];
        switch(&mut a, named("acme"), &session, &FakeHerd(vec![], false)).unwrap();

        let texts: Vec<&str> = a.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["in acme"]);
        assert_eq!(a.title, " scuttlebutt · acme ");
        assert_eq!(a.dir, Some(crate::paths::room_dir(Some("acme")).unwrap()));
    }

    #[test]
    fn a_draft_waits_in_the_room_it_was_typed_in() {
        // Both failure modes at once: a draft following you into another
        // room, and a draft silently discarded on the way out.
        let (_d, _env, session) = scratch();
        let herd = FakeHerd(vec![], false);
        let mut a = app();
        a.room = named("alare");
        a.input = "half-written note for alare".into();

        switch(&mut a, named("acme"), &session, &herd).unwrap();
        assert_eq!(a.input, "");
        a.input = "something for acme".into();

        switch(&mut a, named("alare"), &session, &herd).unwrap();
        assert_eq!(a.input, "half-written note for alare");
        switch(&mut a, named("acme"), &session, &herd).unwrap();
        assert_eq!(a.input, "something for acme");
    }

    #[test]
    fn text_typed_before_a_room_existed_follows_you_into_the_first_one() {
        // A roomless pane can be typed into — `handle_key` only gates the
        // *post* — and `NoneSelected` is unreachable once a room is picked,
        // so stashing that text under it would discard it for good.
        let (_d, _env, session) = scratch();
        let mut a = App {
            input: "typed before picking a room".into(),
            ..App::default()
        };
        switch(&mut a, named("acme"), &session, &FakeHerd(vec![], false)).unwrap();
        assert_eq!(a.input, "typed before picking a room");
    }

    #[test]
    fn a_carried_draft_never_overwrites_one_already_waiting() {
        let (_d, _env, session) = scratch();
        let herd = FakeHerd(vec![], false);
        let mut a = app();
        a.room = named("acme");
        a.input = "acme's own draft".into();
        switch(&mut a, named("alare"), &session, &herd).unwrap();
        switch(&mut a, CurrentRoom::NoneSelected, &session, &herd).unwrap();
        a.input = "typed with no room".into();
        switch(&mut a, named("acme"), &session, &herd).unwrap();
        assert_eq!(a.input, "acme's own draft");
    }

    #[test]
    fn a_switch_that_cannot_open_the_room_changes_nothing_at_all() {
        // The half-applied switch: input taken, draft stashed, the old room
        // marked read, `last_error` cleared — but `app.room` still the old
        // room. The pane then displays and posts to room A while holding
        // room B's draft, which is one company's text one Enter away from
        // another company's room.
        let (_d, _env, session) = scratch();
        let herd = FakeHerd(vec![], false);
        let mut a = app();
        a.room = named("alare");
        a.input = "alare draft".into();
        a.messages = vec![msg("bob", "alare chatter")];
        a.last_error = Some("an earlier failure".into());
        a.unread_seeds.insert(named("alare"), 11);
        a.dir = Some(crate::paths::room_dir(Some("alare")).unwrap());
        a.scroll_from_bottom = 4;
        a.title = title_for(&named("alare"));
        a.members = vec![at("alare-1", "/w/alare/api")];

        // A room whose path is an ordinary file, so reading its log fails
        // where every other step would have succeeded.
        std::fs::write(session.join("wall"), b"not a directory").unwrap();
        let err = switch(&mut a, named("wall"), &session, &herd);
        assert!(err.is_err(), "expected this switch to fail");

        assert_eq!(a.room, named("alare"));
        assert_eq!(a.input, "alare draft");
        assert!(
            a.drafts.is_empty(),
            "a draft was stashed by a failed switch"
        );
        assert_eq!(a.unread_seeds.get(&named("alare")), Some(&11));
        assert_eq!(a.last_error.as_deref(), Some("an earlier failure"));
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.dir, Some(crate::paths::room_dir(Some("alare")).unwrap()));
        // Everything else the switch would have moved, so the guarantee is
        // "nothing changed" rather than "the fields I remembered".
        assert_eq!(a.scroll_from_bottom, 4);
        assert_eq!(a.title, " scuttlebutt · alare ");
        let names: Vec<&str> = a.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["alare-1"]);
    }

    #[test]
    fn a_failed_switch_leaves_no_directory_behind_for_the_room_it_could_not_open() {
        // `room_dir` would have created it before the read could fail, and
        // an empty directory is exactly what `groups::has_history` filters
        // out of every listing.
        let (_d, _env, session) = scratch();
        let mut a = app();
        a.room = named("alare");
        std::fs::write(session.join("wall"), b"not a directory").unwrap();

        // a switch that succeeds does create the room's directory
        assert!(switch(&mut a, named("ghost"), &session, &FakeHerd(vec![], false)).is_ok());
        assert!(session.join("ghost").is_dir());

        assert!(switch(&mut a, named("wall"), &session, &FakeHerd(vec![], false)).is_err());
        assert!(
            !session.join("wall").is_dir(),
            "a failed switch created the room directory anyway"
        );
    }

    #[test]
    fn a_roomless_pane_keeps_its_way_out_even_when_something_has_failed() {
        // Ctrl-K is the only exit from a pane that resolved to no room, and
        // that pane is the case #35 exists to rescue. An error displacing
        // the hint makes it a dead pane again.
        let a = App {
            last_error: Some("could not open that room: Not a directory".into()),
            ..App::default()
        };
        let screen = render(&a, 90, 12).join("\n");
        assert!(screen.contains("Ctrl-K"), "the way out is gone:\n{screen}");
        assert!(screen.contains("could not open that room"), "{screen}");
    }

    #[test]
    fn a_failed_switch_is_never_reported_as_a_failed_post() {
        // The switch is a no-op when it fails, so nothing was posted and
        // nothing was attempted; "post failed" describes an action nobody
        // took.
        let (_d, _env, session) = scratch();
        let mut a = app();
        a.room = named("alare");
        std::fs::write(session.join("wall"), b"not a directory").unwrap();
        let e = switch(&mut a, named("wall"), &session, &FakeHerd(vec![], false)).unwrap_err();
        let reported = format!("could not open that room: {e}");

        // Every arm of the input-line match, not just the one this pane
        // happens to be in: a fixed "post failed" prefix reintroduced in any
        // of them is the same lie.
        for (room, home) in [
            (CurrentRoom::NoneSelected, CurrentRoom::NoneSelected),
            (named("alare"), named("alare")),
            (named("alare"), named("acme")),
        ] {
            let pane = App {
                room,
                home,
                last_error: Some(reported.clone()),
                ..App::default()
            };
            let screen = render(&pane, 90, 12).join("\n");
            assert!(!screen.contains("post failed"), "{screen}");
            assert!(screen.contains("could not open that room"), "{screen}");
        }
    }

    #[test]
    fn a_post_failure_never_hides_which_room_the_post_would_go_to() {
        // The safeguard would otherwise switch itself off at the one moment
        // it is working: the human has just failed to post and is about to
        // retype and press Enter.
        let a = App {
            room: named("acme"),
            home: named("alare"),
            last_error: Some("post failed: disk on fire".into()),
            ..App::default()
        };
        let screen = render(&a, 70, 12).join("\n");
        assert!(screen.contains("acme"), "room name is hidden:\n{screen}");
        assert!(screen.contains("disk on fire"), "{screen}");
    }

    #[test]
    fn switching_returns_to_the_newest_message() {
        let (_d, _env, session) = scratch();
        let mut a = app();
        a.scroll_from_bottom = 7;
        switch(&mut a, named("acme"), &session, &FakeHerd(vec![], false)).unwrap();
        assert_eq!(a.scroll_from_bottom, 0);
    }

    #[test]
    fn a_failed_tail_read_leaves_the_pane_alive_holding_every_draft() {
        // What is lost here is not the read: it is the `?` the run loop
        // applies to it. That Err leaves `run`, ratatui restores the
        // terminal, and `App` is dropped with it — every room's stashed
        // draft included, silently, with nothing written anywhere. The
        // per-room `drafts` map is what turned that from one half-typed line
        // into all of them.
        //
        // A regular file where the room directory belongs is the honest way
        // into the Err branch: it is ENOTDIR for any uid, root included,
        // where `chmod 000` stops failing under a CI running as root and
        // leaves a green no-op. A missing log now has its own guarded error
        // path, so it would not prove that other filesystem errors survive.
        let (_d, _env, session) = scratch();
        let mut a = app();
        a.messages = vec![msg("bob", "already on screen")];
        a.input = "half-typed, not sent".into();
        a.drafts.insert(named("alare"), "unsent to alare".into());
        a.drafts.insert(named("acme"), "unsent to acme".into());
        let wall = session.join("wall");
        std::fs::write(&wall, b"not a directory").unwrap();

        tail(&mut a, &wall);

        // The pane is still here, and it says what failed in its own words:
        // a fixed prefix is what once had a failed switch claiming "post
        // failed".
        let reported = a.last_error.clone();
        assert!(
            reported
                .as_deref()
                .is_some_and(|e| e.contains("could not read this room")),
            "a failed tail read left the pane with nothing to say: {reported:?}"
        );
        // Blanking the list would make a transient read error look like a
        // room somebody emptied.
        assert_eq!(
            a.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["already on screen"]
        );
        assert_eq!(a.input, "half-typed, not sent");
        let mut stashed: Vec<&str> = a.drafts.values().map(String::as_str).collect();
        stashed.sort();
        assert_eq!(stashed, vec!["unsent to acme", "unsent to alare"]);
    }

    #[test]
    fn a_deleted_room_log_keeps_its_history_visible_and_reports_the_read_failure() {
        let (_d, _env, session) = scratch();
        let dir = session.join("acme");
        std::fs::create_dir(&dir).unwrap();
        log_store::append(&dir, "bob", "already on screen").unwrap();
        let mut a = app();
        tail(&mut a, &dir);
        std::fs::remove_file(dir.join("room.jsonl")).unwrap();

        tail(&mut a, &dir);

        assert_eq!(
            a.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["already on screen"]
        );
        assert_eq!(
            a.last_error.as_deref(),
            Some("could not read this room: room log is missing"),
            "the missing room log was silent"
        );
    }

    #[test]
    fn a_room_log_that_has_never_existed_is_silently_empty() {
        let (_d, _env, session) = scratch();
        let dir = session.join("quiet");
        std::fs::create_dir(&dir).unwrap();
        let mut a = app();

        tail(&mut a, &dir);

        assert!(a.messages.is_empty());
        assert_eq!(a.last_error, None);
    }

    #[test]
    fn an_existing_empty_room_log_is_empty_without_an_error() {
        let (_d, _env, session) = scratch();
        let dir = session.join("acme");
        std::fs::create_dir(&dir).unwrap();
        log_store::append(&dir, "bob", "old history").unwrap();
        let mut a = app();
        tail(&mut a, &dir);
        std::fs::write(dir.join("room.jsonl"), b"").unwrap();

        tail(&mut a, &dir);

        assert!(a.messages.is_empty());
        assert_eq!(a.last_error, None);
    }

    #[test]
    fn a_room_log_restored_after_transient_absence_clears_the_error_and_resumes_tailing() {
        let (_d, _env, session) = scratch();
        let dir = session.join("acme");
        std::fs::create_dir(&dir).unwrap();
        log_store::append(&dir, "bob", "before rotation").unwrap();
        let mut a = app();
        tail(&mut a, &dir);
        let log = dir.join("room.jsonl");
        let held = dir.join("room.jsonl.held");
        std::fs::rename(&log, &held).unwrap();
        tail(&mut a, &dir);
        assert!(a.last_error.is_some());

        std::fs::rename(&held, &log).unwrap();
        log_store::append(&dir, "bob", "after rotation").unwrap();
        tail(&mut a, &dir);

        assert_eq!(a.last_error, None);
        assert_eq!(
            a.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["before rotation", "after rotation"]
        );
    }

    #[test]
    fn a_replaced_room_log_is_re_seeded_rather_than_filtered_out_forever() {
        // The other half of what `tail` decides, and the one a reader is
        // most likely to simplify away: with ids restarted below the
        // pane's cursor, reading *since the cursor* returns nothing
        // forever, so the room looks dead while messages arrive.
        let (_d, _env, session) = scratch();
        let dir = session.join("acme");
        std::fs::create_dir(&dir).unwrap();
        for i in 0..3 {
            log_store::append(&dir, "bob", &format!("old{i}")).unwrap();
        }
        let mut a = app();
        tail(&mut a, &dir);
        assert_eq!(a.messages.len(), 3);

        std::fs::remove_file(dir.join("room.jsonl")).unwrap();
        log_store::append(&dir, "bob", "id 1 all over again").unwrap();
        tail(&mut a, &dir);

        assert_eq!(
            a.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["id 1 all over again"]
        );
    }

    #[test]
    fn a_failed_post_is_not_erased_by_the_next_good_read() {
        // The sequence that reaches this: the room's log fails to read
        // (flag set, error drawn), the human presses Enter and the write
        // fails too — after the draw — and the read succeeds again next
        // frame. A flag left standing from the read erases the post failure
        // before it is ever drawn, which is the one thing the flag exists to
        // prevent.
        let (_d, _env, session) = scratch();
        let mut a = app();
        let wall = session.join("wall");
        std::fs::write(&wall, b"not a directory").unwrap();
        tail(&mut a, &wall);

        post(&mut a, &wall, "the line the human typed");
        assert!(
            a.last_error
                .as_deref()
                .is_some_and(|e| e.starts_with("post failed:")),
            "{:?}",
            a.last_error
        );

        let good = session.join("acme");
        std::fs::create_dir(&good).unwrap();
        log_store::append(&good, "bob", "hi").unwrap();
        tail(&mut a, &good);

        assert!(
            a.last_error
                .as_deref()
                .is_some_and(|e| e.starts_with("post failed:")),
            "the good read erased what the human failed to send: {:?}",
            a.last_error
        );
    }

    #[test]
    fn a_failed_switch_is_not_erased_by_the_next_good_read() {
        // Same shape, other writer: `switch_room` returns Err and its caller
        // writes the message, so the flag has to be down by the time it
        // returns — on the failing path as much as the succeeding one.
        let (_d, _env, session) = scratch();
        let mut a = app();
        let wall = session.join("wall");
        std::fs::write(&wall, b"not a directory").unwrap();
        tail(&mut a, &wall);

        let e = switch(&mut a, named("wall"), &session, &FakeHerd(vec![], false)).unwrap_err();
        assert!(
            !a.tail_failed,
            "a failed switch left the read's claim on `last_error` standing"
        );
        // The message the run loop writes for that Err, verbatim.
        a.last_error = Some(format!("could not open that room: {e}"));

        let good = session.join("acme");
        std::fs::create_dir(&good).unwrap();
        tail(&mut a, &good);

        assert_eq!(
            a.last_error.as_deref(),
            Some(format!("could not open that room: {e}").as_str())
        );
    }

    #[test]
    fn trimming_a_room_log_leaves_the_pane_the_history_it_has_read() {
        // A rotation that drops old lines but keeps the newest id: the
        // cursor still matches the file, so there is nothing to re-seed
        // from, and re-reading anyway would shrink the pane's scrollback to
        // whatever the file kept. This is the case that separates the two
        // branches of `reseed`; ids restarting lower is the other.
        let (_d, _env, session) = scratch();
        let dir = session.join("acme");
        std::fs::create_dir(&dir).unwrap();
        for i in 0..5 {
            log_store::append(&dir, "bob", &format!("m{i}")).unwrap();
        }
        let mut a = app();
        tail(&mut a, &dir);
        assert_eq!(a.messages.len(), 5);

        let kept: String = std::fs::read_to_string(dir.join("room.jsonl"))
            .unwrap()
            .lines()
            .skip(3)
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(dir.join("room.jsonl"), kept).unwrap();
        tail(&mut a, &dir);

        assert_eq!(
            a.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["m0", "m1", "m2", "m3", "m4"]
        );
    }

    #[test]
    fn a_room_that_stays_unreadable_keeps_saying_so() {
        // The failure mode this rules out is a pane that complains once and
        // then looks healthy while its log is still unreadable.
        let (_d, _env, session) = scratch();
        let mut a = app();
        let wall = session.join("wall");
        std::fs::write(&wall, b"not a directory").unwrap();

        tail(&mut a, &wall);
        let first = a.last_error.clone();
        tail(&mut a, &wall);

        assert_eq!(a.last_error, first);
        assert!(a.last_error.is_some(), "the room fell quiet about it");
    }

    #[test]
    fn a_room_that_becomes_readable_again_stops_saying_it_failed() {
        // The same path throughout: a room directory that a file was
        // standing in the way of, as it is on disk when the mount or the
        // stray file is cleared up.
        let (_d, _env, session) = scratch();
        let mut a = app();
        let dir = session.join("acme");
        std::fs::write(&dir, b"not a directory").unwrap();
        tail(&mut a, &dir);
        assert!(a.last_error.is_some());

        std::fs::remove_file(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        log_store::append(&dir, "bob", "back again").unwrap();
        tail(&mut a, &dir);

        assert_eq!(a.last_error, None);
        assert_eq!(
            a.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            vec!["back again"]
        );
    }

    #[test]
    fn a_successful_read_does_not_wipe_a_failed_post_before_it_is_seen() {
        // `tail` runs at the top of every loop, before the draw, so a read
        // that cleared `last_error` unconditionally would erase the report
        // of the post the human just failed to send — in the same frame it
        // was written, having never been drawn.
        let (_d, _env, session) = scratch();
        let mut a = app();
        let dir = session.join("acme");
        std::fs::create_dir(&dir).unwrap();
        log_store::append(&dir, "bob", "hi").unwrap();
        a.last_error = Some("post failed: disk on fire".into());

        tail(&mut a, &dir);

        assert_eq!(a.last_error.as_deref(), Some("post failed: disk on fire"));
    }

    #[test]
    fn a_post_failure_does_not_follow_you_into_the_next_room() {
        let (_d, _env, session) = scratch();
        let mut a = app();
        a.last_error = Some("post failed: disk on fire".into());
        switch(&mut a, named("acme"), &session, &FakeHerd(vec![], false)).unwrap();
        assert_eq!(a.last_error, None);
    }

    #[test]
    fn the_roster_changes_with_the_room_rather_than_on_the_next_tick() {
        // The 3-second tick would otherwise leave one room's agent names
        // beside another room's title.
        let (_d, _env, session) = scratch();
        let herd = FakeHerd(
            vec![at("issue-590", "/w/alare/api"), at("acme-x", "/w/acme")],
            false,
        );
        let mut a = app();
        a.members = vec![at("stale", "/w/alare/api")];
        switch_room(
            &mut a,
            named("acme"),
            &session,
            &herd,
            &crate::groups::Grouping::Inactive,
            &mut orgs(fake_org),
        )
        .unwrap();
        let names: Vec<&str> = a.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["acme-x"]);
    }

    #[test]
    fn an_unresolvable_pane_can_pick_its_way_into_a_room() {
        // The state that is dead today: no room selected, nothing to post
        // to, and the picker as the only way out.
        let (_d, _env, session) = scratch();
        let mut a = App::default();
        assert!(a.room.selected().is_none());
        assert_eq!(handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE), None);

        switch(&mut a, named("acme"), &session, &FakeHerd(vec![], false)).unwrap();
        assert_eq!(a.room, named("acme"));
        a.input = "now I can post".into();
        assert_eq!(
            handle_key(&mut a, KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::Post("now I can post".into()))
        );
    }

    #[test]
    fn leaving_a_room_for_no_room_empties_the_pane_rather_than_showing_the_shared_room() {
        // `scoped_members(None, ..)` is the *ungrouped* room's roster, so
        // passing the absence straight through would fill a roomless pane
        // with every agent outside a repository.
        let (_d, _env, session) = scratch();
        let herd = FakeHerd(vec![at("nowhere", "/elsewhere")], false);
        let mut a = app();
        a.room = named("acme");
        a.messages = vec![msg("bob", "acme chatter")];
        a.members = vec![at("acme-x", "/w/acme")];
        switch(&mut a, CurrentRoom::NoneSelected, &session, &herd).unwrap();
        assert!(a.messages.is_empty());
        assert!(a.members.is_empty());
        assert_eq!(a.dir, None);
        assert_eq!(a.title, " scuttlebutt · no room selected ");
    }

    // --- unread dots ---

    fn post_to(room: &str, text: &str) {
        log_store::append(&crate::paths::room_dir(Some(room)).unwrap(), "bob", text).unwrap();
    }

    #[test]
    fn a_room_that_grew_since_the_pane_opened_is_dotted() {
        let (_d, _env, session) = scratch();
        post_to("acme", "history from before");
        let mut a = app();
        a.unread_seeds = seed_unread(&session);
        post_to("acme", "arrived while the pane was open");

        let p = open_picker(
            &a,
            &session,
            &FakeHerd(vec![], false),
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        );
        let acme = p.rows.iter().find(|r| r.room == named("acme")).unwrap();
        assert!(acme.unread, "acme grew since the seed and is not dotted");
    }

    #[test]
    fn history_that_predates_the_pane_is_not_unread() {
        let (_d, _env, session) = scratch();
        post_to("acme", "history from before");
        let mut a = app();
        a.unread_seeds = seed_unread(&session);
        let p = open_picker(
            &a,
            &session,
            &FakeHerd(vec![], false),
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        );
        let acme = p.rows.iter().find(|r| r.room == named("acme")).unwrap();
        assert!(!acme.unread, "a dot here would mean 'history exists'");
    }

    #[test]
    fn the_room_being_read_is_never_dotted() {
        // Its log grows past its seed as you read it.
        let (_d, _env, session) = scratch();
        let mut a = app();
        a.unread_seeds = seed_unread(&session);
        a.room = named("acme");
        post_to("acme", "a message you are looking at");
        let p = open_picker(
            &a,
            &session,
            &FakeHerd(vec![], false),
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        );
        let acme = p.rows.iter().find(|r| r.room == named("acme")).unwrap();
        assert!(!acme.unread);
    }

    #[test]
    fn visiting_a_room_clears_its_dot() {
        let (_d, _env, session) = scratch();
        let herd = FakeHerd(vec![], false);
        let mut a = app();
        a.unread_seeds = seed_unread(&session);
        a.room = named("alare");
        post_to("acme", "unread while you were in alare");

        switch(&mut a, named("acme"), &session, &herd).unwrap();
        switch(&mut a, named("alare"), &session, &herd).unwrap();

        let p = open_picker(
            &a,
            &session,
            &herd,
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        );
        let acme = p.rows.iter().find(|r| r.room == named("acme")).unwrap();
        assert!(!acme.unread, "acme was visited and should be caught up");
    }

    #[test]
    fn a_truncated_room_is_still_dotted() {
        // `len > seed` never fires again once a log is replaced by a shorter
        // one, leaving that room permanently silent however much arrives.
        let (_d, _env, session) = scratch();
        post_to(
            "acme",
            "a long line of history that will be replaced wholesale",
        );
        let mut a = app();
        a.unread_seeds = seed_unread(&session);
        a.room = named("alare");
        std::fs::write(
            crate::paths::room_dir(Some("acme"))
                .unwrap()
                .join("room.jsonl"),
            b"",
        )
        .unwrap();
        post_to("acme", "short");

        let p = open_picker(
            &a,
            &session,
            &FakeHerd(vec![], false),
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        );
        let acme = p.rows.iter().find(|r| r.room == named("acme")).unwrap();
        assert!(
            acme.unread,
            "a replaced log left the room permanently silent"
        );
    }

    #[test]
    fn a_room_created_after_the_pane_opened_is_dotted() {
        let (_d, _env, session) = scratch();
        let a = App {
            unread_seeds: seed_unread(&session),
            room: named("alare"),
            ..App::default()
        };
        post_to("acme", "a room that did not exist at pane start");
        let p = open_picker(
            &a,
            &session,
            &FakeHerd(vec![], false),
            &crate::groups::Grouping::Inactive,
            &mut orgs(no_org),
        );
        let acme = p.rows.iter().find(|r| r.room == named("acme")).unwrap();
        assert!(acme.unread);
    }

    #[test]
    fn picker_rows_take_their_provenance_from_the_order_they_are_listed_in() {
        // One derivation, shared with the sort: a row labelled "config"
        // sitting above a row with agents in it is the drift `Room::sources`
        // exists to prevent.
        let (_d, _env, session) = scratch();
        post_to("acme", "history only");
        let herd = FakeHerd(vec![at("alare-1", "/w/alare/api")], false);
        let a = App::default();
        let p = open_picker(
            &a,
            &session,
            &herd,
            &crate::groups::Grouping::Inactive,
            &mut orgs(fake_org),
        );
        let labelled: Vec<(&str, &str)> = p
            .rows
            .iter()
            .map(|r| (r.room.label(), r.sources.as_str()))
            .collect();
        assert_eq!(
            labelled,
            vec![("alare", "live agents"), ("acme", "history")]
        );
    }

    #[test]
    fn a_wide_room_name_does_not_push_the_picker_columns_out_of_line() {
        // `{:<20}` pads by chars; a CJK name is twice as wide as it is long,
        // so its detail column would start four cells late.
        let mut a = app();
        a.picker = Some(PickerState {
            rows: ["一二三四", "acme"]
                .iter()
                .map(|n| PickerRow {
                    room: CurrentRoom::Named((*n).into()),
                    agents: 0,
                    sources: "config".into(),
                    unread: false,
                })
                .collect(),
            filter: String::new(),
            cursor: 0,
        });
        let screen = render(&a, 90, 12).join("\n");
        let columns = columns_of(&a, 90, 12, "0 agents");
        assert_eq!(columns.len(), 2, "expected both rows:\n{screen}");
        assert_eq!(
            columns[0], columns[1],
            "detail columns are misaligned:\n{screen}"
        );
    }

    #[test]
    fn the_picker_survives_a_pane_too_small_to_hold_it() {
        let mut a = app();
        a.picker = Some(picker_of(&["alare"]));
        for (w, h) in [(1, 1), (4, 2), (10, 3), (0, 0)] {
            render(&a, w, h);
        }
    }

    #[test]
    fn backspace_deletes() {
        let mut a = app();
        a.input = "hi".into();
        handle_key(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(a.input, "h");
    }
}
