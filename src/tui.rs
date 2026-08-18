use crate::herd::{AgentInfo, HerdControl, RealHerd};
use crate::log_store::{self, Message};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

#[derive(Default)]
pub struct App {
    pub messages: Vec<Message>,
    pub input: String,
    pub members: Vec<AgentInfo>,
    pub scroll_from_bottom: u16,
    pub quit: bool,
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

pub fn run() -> Result<()> {
    let dir = crate::paths::room_dir()?;
    let herd = RealHerd;
    let mut app = App {
        messages: log_store::read_since(&dir, 0)?,
        members: herd.list_agents().unwrap_or_default(),
        ..App::default()
    };

    let mut terminal = ratatui::init();
    let mut last_member_refresh = std::time::Instant::now();
    let result = (|| -> Result<()> {
        while !app.quit {
            // tail new messages every loop; members on a slow tick
            let last = app.messages.last().map(|m| m.id).unwrap_or(0);
            let mut fresh = log_store::read_since(&dir, last)?;
            app.messages.append(&mut fresh);
            if last_member_refresh.elapsed() > std::time::Duration::from_secs(3) {
                if let Ok(m) = herd.list_agents() {
                    app.members = m;
                }
                last_member_refresh = std::time::Instant::now();
            }

            terminal.draw(|f| draw(f, &app))?;

            if event::poll(std::time::Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        if let Some(text) = handle_key(&mut app, key.code, key.modifiers) {
                            log_store::append(&dir, "human", &text)?;
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

    let lines: Vec<Line> = app
        .messages
        .iter()
        .map(|m| {
            let who_style = if m.from == "human" {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            };
            Line::from(vec![
                Span::styled(format!("{}: ", m.from), who_style),
                Span::raw(m.text.clone()),
            ])
        })
        .collect();
    let total = lines.len() as u16;
    let visible = top[0].height.saturating_sub(2);
    let bottom_offset = total.saturating_sub(visible);
    let scroll = bottom_offset.saturating_sub(app.scroll_from_bottom.min(bottom_offset));
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" scuttlebutt "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
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

    f.render_widget(
        Paragraph::new(app.input.as_str())
            .block(Block::default().borders(Borders::ALL).title(" message (Enter to send, Esc to quit) ")),
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
    fn ctrl_c_and_esc_quit() {
        let mut a = app();
        handle_key(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.quit);
        let mut b = app();
        handle_key(&mut b, KeyCode::Esc, KeyModifiers::NONE);
        assert!(b.quit);
    }

    #[test]
    fn backspace_deletes() {
        let mut a = app();
        a.input = "hi".into();
        handle_key(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(a.input, "h");
    }
}
