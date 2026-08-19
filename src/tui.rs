use crate::herd::{AgentInfo, HerdControl, RealHerd};
use crate::log_store::{self, Message};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub members: Vec<AgentInfo>,
    pub scroll_from_bottom: u16,
    pub quit: bool,
    /// Last post failure, surfaced in the input-line title. A transient write
    /// failure must not tear the chat pane down.
    pub post_error: Option<String>,
    /// Message-pane title, always naming the resolved group so the human
    /// can never mistake which room's input line they are typing into.
    pub title: String,
}

pub fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Option<String> {
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
            app.quit = true;
            None
        }
        (KeyCode::Enter, _) => {
            if app.input.trim().is_empty() {
                None
            } else {
                let text = std::mem::take(&mut app.input);
                app.scroll_from_bottom = 0;
                Some(text)
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

/// Decide whether the in-memory tail cursor is stale relative to the log
/// file's own last id. If the file's last id is lower than our cursor, the
/// log was truncated or replaced (e.g. a test fixture reset, or a future
/// "clear room" feature) and ids restarted from 1; tailing from the old
/// cursor would filter out every subsequent message forever. In that case
/// the caller should discard its in-memory messages and re-seed from the
/// start of the file.
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

/// Message-pane title. Always names the resolved group, so it is the
/// visible safeguard against typing into the wrong company's room.
fn title_for(resolved: Option<&str>) -> String {
    match resolved {
        Some(g) => format!(" scuttlebutt · {g} "),
        None => " scuttlebutt ".to_string(),
    }
}

/// The members pane's roster, scoped to the resolved group. Both the initial
/// seed and the periodic refresh go through here: the pane sits beside a title
/// naming the group, so an unscoped roster would show one company's agent
/// names in another company's room. `None` on a failed listing, so the
/// refresh can keep the last known roster instead of blanking the pane for a
/// transient `herdr agent list` failure.
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

pub fn run(group: Option<&str>) -> Result<()> {
    let grouping = crate::groups::load(&crate::paths::base_dir()?);
    let mut orgs = crate::git_org::OrgCache::default();
    let resolved =
        crate::cli::resolve_group(group, &std::env::current_dir()?, &grouping, &mut orgs)?;
    let dir = crate::paths::room_dir(resolved.as_deref())?;
    let title = title_for(resolved.as_deref());
    let herd = RealHerd;
    let mut app = App {
        messages: log_store::read_since(&dir, 0)?,
        members: scoped_members(&herd, resolved.as_deref(), &grouping, &mut orgs)
            .unwrap_or_default(),
        title,
        ..App::default()
    };

    let mut terminal = ratatui::init();
    let mut last_member_refresh = std::time::Instant::now();
    let result = (|| -> Result<()> {
        while !app.quit {
            // tail new messages every loop; members on a slow tick
            let last = app.messages.last().map(|m| m.id).unwrap_or(0);
            let file_last = log_store::last_id(&dir)?;
            if should_reseed(last, file_last) {
                // room.jsonl was truncated or replaced (ids restarted lower
                // than our cursor); discard the stale in-memory tail and
                // re-seed from the start of the file instead of filtering
                // every future message out forever.
                app.messages = log_store::read_since(&dir, 0)?;
            } else {
                let mut fresh = log_store::read_since(&dir, last)?;
                app.messages.append(&mut fresh);
            }
            if last_member_refresh.elapsed() > std::time::Duration::from_secs(3) {
                if let Some(m) = scoped_members(&herd, resolved.as_deref(), &grouping, &mut orgs) {
                    app.members = m;
                }
                last_member_refresh = std::time::Instant::now();
            }

            terminal.draw(|f| draw(f, &app))?;

            if event::poll(std::time::Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        if let Some(text) = handle_key(&mut app, key.code, key.modifiers) {
                            app.post_error = match log_store::append(&dir, "human", &text) {
                                Ok(_) => None,
                                Err(e) => Some(e.to_string()),
                            };
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

    let input_title = match &app.post_error {
        Some(e) => format!(" post failed: {e} "),
        None => " message (Enter to send, Esc to quit) ".to_string(),
    };
    f.render_widget(
        Paragraph::new(app.input.as_str())
            .block(Block::default().borders(Borders::ALL).title(input_title)),
        outer[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn app() -> App {
        App::default()
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
        assert_eq!(submitted.as_deref(), Some("hello"));
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
        fn prompt(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn at(name: &str, cwd: &str) -> AgentInfo {
        AgentInfo {
            name: name.into(),
            pane_id: "w1:p1".into(),
            status: "idle".into(),
            cwd: cwd.into(),
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
        assert!(title_for(Some("alare")).contains("alare"));
    }

    #[test]
    fn title_for_ungrouped_has_no_stray_group_label() {
        let title = title_for(None);
        assert!(!title.contains("·"));
        assert!(title.contains("scuttlebutt"));
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
    fn post_error_is_surfaced_in_the_input_title() {
        let app = App {
            post_error: Some("disk on fire".into()),
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

    #[test]
    fn backspace_deletes() {
        let mut a = app();
        a.input = "hi".into();
        handle_key(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(a.input, "h");
    }
}
