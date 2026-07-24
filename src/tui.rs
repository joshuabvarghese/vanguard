//! Flight Deck TUI, ported from `internal/tui/flightdeck.go`. Four live
//! quadrants (tenants, namespaces, reconcile logs, chaos panel) rendered
//! with `ratatui`, refreshed on a tick timer + store event bus.

use crate::store::{Store, TenantRecord};
use crossterm::event::{self, Event as CEvent, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::{Frame, Terminal};
use std::io;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_millis(250);

/// Rune-safe truncation: log lines are full of multi-byte glyphs (✓ ⟳ 💥 ⚠),
/// so truncating on byte offsets (as the Go version originally did, before
/// that bug was fixed) can split a UTF-8 character and panic or corrupt the
/// terminal. Truncating a `&str` slice on a non-char-boundary index panics in
/// Rust, so this function is the direct analogue of that Go regression test.
fn truncate_display(s: &str, max_width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_width {
        return s.to_string();
    }
    let cut = max_width.saturating_sub(1);
    let mut out: String = chars[..cut.min(chars.len())].iter().collect();
    out.push('…');
    out
}

fn colorise(line: &str) -> Style {
    if line.contains('✓') {
        Style::default().fg(Color::Green)
    } else if line.contains('✗') || line.contains("DEGRADED") {
        Style::default().fg(Color::Red)
    } else if line.contains('💥') {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if line.contains('⚠') {
        Style::default().fg(Color::Yellow)
    } else if line.contains('⟳') {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Gray)
    }
}

pub struct App {
    store: Arc<Store>,
    version: String,
    tenants: Vec<TenantRecord>,
    logs: Vec<String>,
    tick: u64,
}

impl App {
    pub fn new(store: Arc<Store>, version: impl Into<String>) -> Self {
        Self {
            store,
            version: version.into(),
            tenants: Vec::new(),
            logs: Vec::new(),
            tick: 0,
        }
    }

    fn refresh(&mut self) {
        self.tenants = self.store.list_tenants();
        self.logs = self.store.logs();
        self.tick += 1;
    }

    fn draw(&self, f: &mut Frame) {
        let size = f.area();
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(size);

        self.draw_header(f, root[0]);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(root[1]);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(cols[0]);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(cols[1]);

        self.draw_tenants(f, left[0]);
        self.draw_logs(f, left[1]);
        self.draw_namespace_matrix(f, right[0]);
        self.draw_chaos_panel(f, right[1]);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let pulse = if self.tick % 2 == 0 { "◆" } else { "◇" };
        let title = format!(
            "  {pulse}  VANGUARD FLIGHT DECK  •  Control Plane {}  •  {} tenants active",
            self.version,
            self.tenants.len()
        );
        let p = Paragraph::new(title)
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(p, area);
    }

    fn draw_tenants(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.tenants.is_empty() {
            vec![ListItem::new("  (no tenants yet — POST /api/v1/tenants)")]
        } else {
            self.tenants
                .iter()
                .map(|t| {
                    let phase_style = match t.phase {
                        crate::store::TenantPhase::Ready => Style::default().fg(Color::Green),
                        crate::store::TenantPhase::Degraded => Style::default().fg(Color::Red),
                        crate::store::TenantPhase::Provisioning => {
                            Style::default().fg(Color::Yellow)
                        }
                        crate::store::TenantPhase::Terminating => {
                            Style::default().fg(Color::DarkGray)
                        }
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("  {:<16}", t.tenant_id)),
                        Span::styled(format!("{:<12}", t.phase.as_str()), phase_style),
                        Span::raw(format!(
                            "tier={:<10} rps={:<6} reconciles={}",
                            t.tier, t.rps, t.reconcile_count
                        )),
                    ]))
                })
                .collect()
        };
        let list = List::new(items).block(
            Block::default()
                .title(" ◈ TENANT PIPELINES ")
                .borders(Borders::ALL),
        );
        f.render_widget(list, area);
    }

    fn draw_namespace_matrix(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.tenants.is_empty() {
            vec![ListItem::new(
                "  (namespaces appear once a tenant reconciles)",
            )]
        } else {
            self.tenants
                .iter()
                .map(|t| {
                    ListItem::new(format!(
                        "  ns/{:<20} proxy={}",
                        t.namespace, t.proxy_pod_name
                    ))
                })
                .collect()
        };
        let list = List::new(items).block(
            Block::default()
                .title(" ⬡ NAMESPACE MATRIX ")
                .borders(Borders::ALL),
        );
        f.render_widget(list, area);
    }

    fn draw_logs(&self, f: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4) as usize;
        let visible = self
            .logs
            .iter()
            .rev()
            .take(area.height.saturating_sub(2) as usize)
            .rev();
        let lines: Vec<Line> = visible
            .map(|l| {
                let truncated = truncate_display(l, width.max(1));
                Line::from(Span::styled(truncated, colorise(l)))
            })
            .collect();
        let p = Paragraph::new(lines).block(
            Block::default()
                .title(" ⟳ RECONCILE LOOP LOGS ")
                .borders(Borders::ALL),
        );
        f.render_widget(p, area);
    }

    fn draw_chaos_panel(&self, f: &mut Frame, area: Rect) {
        let mut text = vec![
            Line::from("  curl -X POST /api/v1/tenants/<id>/chaos/kill-proxy"),
            Line::from(""),
        ];
        for t in &self.tenants {
            text.push(Line::from(format!(
                "  {} → {}",
                t.tenant_id,
                t.phase.as_str()
            )));
        }
        let p = Paragraph::new(text).block(
            Block::default()
                .title(" 💥 CHAOS ACTION PANEL ")
                .borders(Borders::ALL),
        );
        f.render_widget(p, area);
    }
}

pub async fn run(
    store: Arc<Store>,
    version: String,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(store, version);
    let result = run_loop(&mut terminal, &mut app, &mut shutdown).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        app.refresh();
        terminal.draw(|f| app.draw(f))?;

        if event::poll(TICK)? {
            if let CEvent::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL))
                {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_display_is_char_boundary_safe_on_emoji() {
        let line = "  ⟳  [acme] reconcile #1 start — provisioning namespace, configmap, and proxy deployment 💥⚠✓";
        // Would panic on a byte-index slice landing mid-codepoint; must not here.
        let out = truncate_display(line, 20);
        assert!(out.chars().count() <= 20);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_display_leaves_short_lines_untouched() {
        let line = "short line";
        assert_eq!(truncate_display(line, 50), line);
    }

    #[test]
    fn colorise_maps_categories() {
        assert_eq!(colorise("  ✓ ready").fg, Some(Color::Green));
        assert_eq!(colorise("  ✗ DEGRADED").fg, Some(Color::Red));
        assert_eq!(colorise("  💥 chaos").fg, Some(Color::Magenta));
        assert_eq!(colorise("  ⚠ drift").fg, Some(Color::Yellow));
        assert_eq!(colorise("  ⟳ reconcile").fg, Some(Color::Cyan));
        assert_eq!(colorise("  plain info").fg, Some(Color::Gray));
    }

    #[test]
    fn app_refresh_handles_empty_store_without_panicking() {
        let store = Arc::new(Store::new());
        let mut app = App::new(store, "test");
        app.refresh();
        assert_eq!(app.tenants.len(), 0);
    }
}
