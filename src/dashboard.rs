//! Full-screen dashboard — bare `agentbridge` (no subcommand) opens this when
//! stdin is a terminal. One table of every tool's sessions, one-key sync and
//! pull, provider filter. Same binary-only rule as `tui.rs`: the library
//! stays UI-free; this module drives it through plain public APIs
//! (`discover`, `sync_into`, `pull_back`).
//!
//! `pull` here is deliberately non-interactive (`AutoMerge` semantics —
//! `pull_back` without a resolver): the dashboard is already a TUI, it cannot
//! open the conflict screen from `tui.rs` inside itself. When conflicts
//! exist, the status line says so and points at a real `agentbridge pull` in
//! a terminal.

use agentbridge::connector::Registry;
use agentbridge::connectors;
use agentbridge::index::{discover, IndexEntry};
use agentbridge::sync::{pull_back, sync_into};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};
use std::io;

/// Characters per column before truncation.
const TITLE_MAX: usize = 42;
const PROJECT_MAX: usize = 24;

/// ASCII brand mark: two agents connected by a bridge.
const BRIDGE_LOGO_1: &str = "  ╔═════════════╗                ╔═════════════╗  ";
const BRIDGE_LOGO_2: &str = "  ║   agent A   ║═════ bridge ════║   agent B   ║  ";
const BRIDGE_LOGO_3: &str = "  ╚═════════════╝                ╚═════════════╝  ";

pub struct Dashboard {
    registry: Registry,
    entries: Vec<IndexEntry>,
    scan_errors: Vec<String>,
    selected: usize,
    provider: Option<String>,
    status: String,
    unsync_pending: bool,
}

impl Dashboard {
    pub fn new() -> Self {
        let registry = connectors::all();
        let (entries, scan_errors) = index_snapshot(&registry);
        Self {
            registry,
            entries,
            scan_errors,
            selected: 0,
            provider: None,
            status: "ready — s sync, p pull, u unsync, Tab filter, ↑/↓ move, q quit".to_string(),
            unsync_pending: false,
        }
    }

    /// Indices into `entries` matching the active provider filter, newest
    /// session first.
    fn filtered(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.provider.as_deref().is_none_or(|p| e.provider == p))
            .map(|(i, _)| i)
            .collect();
        idx.sort_by_key(|&i| {
            std::cmp::Reverse(
                self.entries[i]
                    .last_event_at
                    .or(self.entries[i].started_at)
                    .unwrap_or(chrono::DateTime::from_timestamp(0, 0).unwrap()),
            )
        });
        idx
    }

    fn refresh(&mut self) {
        let (entries, scan_errors) = index_snapshot(&self.registry);
        self.entries = entries;
        self.scan_errors = scan_errors;
        if self.selected >= self.filtered().len() {
            self.selected = self.filtered().len().saturating_sub(1);
        }
    }

    fn providers(&self) -> Vec<String> {
        self.registry.detected().map(|c| c.id().to_string()).collect()
    }

    fn do_sync(&mut self) {
        let dir = std::env::current_dir().unwrap_or_default();
        let pulled = pull_back(false);
        let n: usize = pulled.pulled.iter().map(|(_, n)| n).sum();
        let report = sync_into(&self.registry, &dir, false);
        let conflicts = pulled.conflicts.len();
        let mut msg = format!(
            "sync {}: {} created, {} unchanged, {} skipped-native",
            dir.display(),
            report.created.len(),
            report.unchanged,
            report.skipped_native
        );
        if report.codex_indexed > 0 {
            msg.push_str(&format!(", {} codex-indexed", report.codex_indexed));
        }
        if n > 0 {
            msg.push_str(&format!("; pulled {} turn(s)", n));
        }
        if conflicts > 0 {
            msg.push_str(&format!(
                "; {} conflict(s) auto-merged — run `agentbridge pull` in a terminal to choose",
                conflicts
            ));
        }
        if !report.errors.is_empty() || !pulled.errors.is_empty() {
            let total = report.errors.len() + pulled.errors.len();
            msg.push_str(&format!("; {total} error(s) — see `agentbridge status`"));
        }
        self.status = msg;
        self.refresh();
    }

    fn do_pull(&mut self) {
        self.unsync_pending = false;
        let report = pull_back(false);
        let n: usize = report.pulled.iter().map(|(_, n)| n).sum();
        let conflicts = report.conflicts.len();
        let mut msg = format!(
            "pull: recovered {} turn(s) from {} session(s){}",
            n,
            report.pulled.len(),
            if report.renamed.is_empty() { String::new() } else { format!(", {} rename(s)", report.renamed.len()) }
        );
        if conflicts > 0 {
            msg.push_str(&format!(
                "; {} conflict(s) auto-merged — run `agentbridge pull` in a terminal to choose",
                conflicts
            ));
        }
        self.status = msg;
        self.refresh();
    }

    fn preview_unsync(&mut self) {
        let report = agentbridge::sync::unsync(true);
        self.status = format!(
            "unsync would remove {} file(s), keep {} foreign (touched by a tool), {} already missing — press y to confirm",
            report.removed.len(),
            report.kept_foreign.len(),
            report.missing
        );
        self.unsync_pending = true;
    }

    fn confirm_unsync(&mut self) {
        let report = agentbridge::sync::unsync(false);
        self.status = format!(
            "unsync: removed {} file(s){}",
            report.removed.len(),
            if report.kept_foreign.is_empty() && report.missing == 0 {
                String::new()
            } else {
                format!(", kept {} foreign, {} missing", report.kept_foreign.len(), report.missing)
            }
        );
        self.unsync_pending = false;
        self.refresh();
    }

    fn cycle_filter(&mut self) {
        let providers = self.providers();
        self.provider = next_provider(self.provider.as_deref(), &providers);
        self.selected = 0;
    }

    pub fn run(&mut self) -> io::Result<()> {
        let _guard = TerminalGuard::new()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

        loop {
            terminal.draw(|f| draw(f, self))?;

            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.unsync_pending = false;
                    self.move_selection(-1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.unsync_pending = false;
                    self.move_selection(1);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.unsync_pending = false;
                    self.selected = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.unsync_pending = false;
                    self.selected = self.filtered().len().saturating_sub(1);
                }
                KeyCode::Char('s') => {
                    self.unsync_pending = false;
                    self.do_sync();
                }
                KeyCode::Char('p') => self.do_pull(),
                KeyCode::Char('u') => self.preview_unsync(),
                KeyCode::Char('y') if self.unsync_pending => self.confirm_unsync(),
                KeyCode::Tab => {
                    self.unsync_pending = false;
                    self.cycle_filter();
                }
                KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                _ => {}
            }
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as i64 + delta as i64).rem_euclid(len as i64) as usize;
    }
}

fn index_snapshot(registry: &Registry) -> (Vec<IndexEntry>, Vec<String>) {
    let index = discover(registry);
    (index.entries, index.errors)
}

/// Next filter in a cycle that starts at "all providers" (`None`) and then
/// visits each detected provider in registry order, wrapping.
fn next_provider(current: Option<&str>, providers: &[String]) -> Option<String> {
    let len = providers.len();
    let pos = current
        .and_then(|c| providers.iter().position(|p| p == c))
        .map(|i| (i + 1) % len)
        .unwrap_or(0);
    if pos == 0 && current.is_some() {
        return None;
    }
    providers.get(pos).cloned()
}

/// Human-friendly age of a timestamp: "just now", "3m", "2h", "5d", else the
/// UTC date.
fn relative_time(dt: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let Some(dt) = dt else { return "-".to_string() };
    let now = chrono::Utc::now();
    let secs = (now - dt).num_seconds().max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 86400 * 30 {
        format!("{}d", secs / 86400)
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

/// Character-safe truncation with an ellipsis; short strings pass through.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

fn provider_color(id: &str) -> Color {
    match id {
        "claude-code" => Color::LightCyan,
        "codex-cli" => Color::LightGreen,
        "opencode" => Color::LightMagenta,
        "antigravity" => Color::LightYellow,
        _ => Color::Gray,
    }
}

fn draw(f: &mut Frame, d: &Dashboard) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let filter = d.provider.as_deref().unwrap_or("all");
    let detected = d
        .registry
        .detected()
        .map(|c| format!("{} {}", if c.detect() { "✓" } else { "✗" }, c.id()))
        .collect::<Vec<_>>()
        .join("  ");
    let header = Paragraph::new(vec![
        Line::from(Span::styled(BRIDGE_LOGO_1, Style::default().fg(Color::Cyan))),
        Line::from(Span::styled(BRIDGE_LOGO_2, Style::default().fg(Color::Cyan))),
        Line::from(Span::styled(BRIDGE_LOGO_3, Style::default().fg(Color::Cyan))),
        Line::from(Span::styled(
            format!(" filter: {}    detected: {}", filter, detected),
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL).title(" agentbridge — bridge between your agents "));
    f.render_widget(header, rows[0]);

    let filtered = d.filtered();
    let list: Vec<Row> = filtered
        .iter()
        .map(|&i| {
            let e = &d.entries[i];
            let title = truncate(
                e.title.as_deref().unwrap_or(e.id.as_str()),
                TITLE_MAX,
            );
            let project = e
                .project_path
                .as_deref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("-");
            let project = truncate(project, PROJECT_MAX);
            let updated = relative_time(e.last_event_at.or(e.started_at));
            Row::new(vec![
                Cell::from(Span::styled(&e.provider, Style::default().fg(provider_color(&e.provider)))),
                Cell::from(Span::styled(title, Style::default().add_modifier(Modifier::BOLD))),
                Cell::from(project),
                Cell::from(updated),
            ])
        })
        .collect();

    let table = Table::new(list, [
        Constraint::Length(13),
        Constraint::Min(20),
        Constraint::Min(12),
        Constraint::Length(9),
    ])
    .block(Block::default().borders(Borders::ALL).title(format!(
        " sessions — {} shown of {} across {} tools ",
        filtered.len(),
        d.entries.len(),
        d.registry.detected().count()
    )))
    .row_highlight_style(
        Style::default()
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("> ");

    let mut state = TableState::default();
    state.select(Some(d.selected.min(filtered.len().saturating_sub(1))));
    f.render_stateful_widget(table, rows[1], &mut state);

    let mut lines: Vec<Line> = Vec::new();
    let status = Line::from(Span::styled(
        &d.status,
        Style::default().fg(if d.status.contains("error") { Color::LightRed } else { Color::LightBlue }),
    ));
    lines.push(status);
    if !d.scan_errors.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {} unreadable session(s) — see `agentbridge ls` stderr", d.scan_errors.len()),
            Style::default().fg(Color::DarkGray),
        )));
    }
    let status_para = Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(status_para, rows[2]);

    let footer = Paragraph::new(if d.unsync_pending {
        "press y to run unsync for real (removes only agentbridge's own files) — any other key cancels"
    } else {
        "↑/↓ or j/k move   g/G top/bottom   s sync   p pull   u unsync   Tab filter   q/Esc quit"
    });
    f.render_widget(footer, rows[3]);
}

/// Restores the terminal on drop, including on panic or early return.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_provider_cycles_all_then_each_provider_then_wraps() {
        let providers = vec!["claude-code".to_string(), "codex-cli".to_string()];
        assert_eq!(next_provider(None, &providers), Some("claude-code".to_string()));
        assert_eq!(next_provider(Some("claude-code"), &providers), Some("codex-cli".to_string()));
        assert_eq!(next_provider(Some("codex-cli"), &providers), None);
        assert_eq!(next_provider(None, &providers), Some("claude-code".to_string()));
        // Unknown provider (e.g. one since disconnected) falls back to
        // "all" — never panics, never loops forever.
        assert_eq!(next_provider(Some("ghost"), &providers), None);
    }

    #[test]
    fn test_next_provider_empty_list_stays_none() {
        let providers: Vec<String> = vec![];
        assert_eq!(next_provider(None, &providers), None);
    }

    #[test]
    fn test_relative_time_covers_every_band() {
        let now = chrono::Utc::now();
        assert_eq!(relative_time(None), "-");
        assert_eq!(relative_time(Some(now)), "just now");
        assert_eq!(relative_time(Some(now - chrono::Duration::minutes(3))), "3m");
        assert_eq!(relative_time(Some(now - chrono::Duration::hours(2))), "2h");
        assert_eq!(relative_time(Some(now - chrono::Duration::days(5))), "5d");
        let old = now - chrono::Duration::days(200);
        assert_eq!(relative_time(Some(old)), old.format("%Y-%m-%d").to_string());
    }

    #[test]
    fn test_truncate_is_char_safe_and_marks_cuts() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("x", 0), "");
        assert_eq!(truncate("héllo wörld", 4), "hél…");
        // No panics on absurd inputs.
        assert_eq!(truncate("", 3), "");
    }
}
