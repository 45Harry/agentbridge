//! Full-screen terminal GUI for the `agentbridge pull` write-back conflict
//! screen: a session was continued in more than one tool since the last
//! pull, and the operator picks what to keep. Rust equivalent of a
//! double-buffered TUI toolkit (`ratatui` + `crossterm`), binary-only — the
//! library (`sync.rs`) stays free of any UI dependency via the
//! `ConflictResolver` trait.

use agentbridge::sync::{ConflictChoice, ConflictItem, ConflictResolver};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io;

pub struct RatatuiConflictResolver;

impl ConflictResolver for RatatuiConflictResolver {
    fn resolve(&mut self, session_id: &str, items: &[ConflictItem]) -> ConflictChoice {
        // A broken terminal (draw/read error) must never lose data — fall
        // back to Skip so the same conflict is offered again next pull,
        // never MergeAll (that would silently apply a choice nobody made).
        run(session_id, items).unwrap_or(ConflictChoice::Skip)
    }
}

/// Restores the terminal on drop, including on an early return or panic —
/// never leave the operator's shell in raw/alternate-screen mode.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn menu_items(items: &[ConflictItem]) -> Vec<String> {
    let mut menu = vec!["Merge — keep new work from every tool".to_string()];
    for item in items {
        menu.push(format!("Keep only {} — discard the other tool(s)' new turns", item.provider));
    }
    menu.push("Skip — decide on the next pull".to_string());
    menu
}

fn run(session_id: &str, items: &[ConflictItem]) -> io::Result<ConflictChoice> {
    let _guard = TerminalGuard::new()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let menu = menu_items(items);
    let mut selected = 0usize;

    loop {
        terminal.draw(|f| draw(f, session_id, items, &menu, selected))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = if selected == 0 { menu.len() - 1 } else { selected - 1 };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1) % menu.len();
            }
            KeyCode::Enter => {
                return Ok(if selected == 0 {
                    ConflictChoice::MergeAll
                } else if selected == menu.len() - 1 {
                    ConflictChoice::Skip
                } else {
                    ConflictChoice::KeepOnly(items[selected - 1].provider.clone())
                });
            }
            KeyCode::Esc | KeyCode::Char('q') => return Ok(ConflictChoice::Skip),
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, session_id: &str, items: &[ConflictItem], menu: &[String], selected: usize) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(menu.len() as u16 + 2),
            Constraint::Length(1),
        ])
        .split(area);

    let title = Paragraph::new(format!(
        "Session {} was continued in {} tools since the last pull — nothing is written until you confirm.",
        session_id,
        items.len()
    ))
    .wrap(Wrap { trim: true })
    .block(Block::default().borders(Borders::ALL).title(" agentbridge — write-back conflict "));
    f.render_widget(title, rows[0]);

    let panel_pct = (100 / items.len().max(1)) as u16;
    let panel_constraints: Vec<Constraint> = items.iter().map(|_| Constraint::Percentage(panel_pct)).collect();
    let panels = Layout::default().direction(Direction::Horizontal).constraints(panel_constraints).split(rows[1]);

    for (i, item) in items.iter().enumerate() {
        let Some(panel_area) = panels.get(i) else { continue };
        let mut lines: Vec<Line> = Vec::new();
        if let Some(t) = &item.new_title {
            lines.push(Line::from(Span::styled(
                format!("renamed to: {}", t),
                Style::default().fg(Color::Yellow),
            )));
        }
        for m in &item.new_messages {
            let text = m
                .text
                .as_deref()
                .or(m.tool_name.as_deref())
                .unwrap_or("(no text)");
            lines.push(Line::from(vec![
                Span::styled(format!("{:?}: ", m.role), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(text.to_string()),
            ]));
        }
        if lines.is_empty() {
            lines.push(Line::from("(no new turns)"));
        }
        let panel_title = format!(" {} ({} new) ", item.provider, item.new_messages.len());
        let p = Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(panel_title));
        f.render_widget(p, *panel_area);
    }

    let list_items: Vec<ListItem> = menu.iter().map(|m| ListItem::new(m.as_str())).collect();
    let list = List::new(list_items)
        .block(Block::default().borders(Borders::ALL).title(" What should agentbridge keep? "))
        .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select(Some(selected));
    f.render_stateful_widget(list, rows[2], &mut state);

    let footer = Paragraph::new("↑/↓ or j/k move   Enter confirm   Esc/q skip (decide later)");
    f.render_widget(footer, rows[3]);
}
