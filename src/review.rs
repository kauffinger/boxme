use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::netcap::NetworkContact;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    /// "vendor/: 4321 files" — inside the expected write-set, shown as a count.
    ExpectedSummary,
    /// Expected file (composer.lock etc.) that changed.
    ExpectedFile,
    Added,
    Modified,
    Deleted,
    Binary,
}

#[derive(Debug, Clone)]
pub struct FileItem {
    pub label: String,
    pub kind: FileKind,
    pub diff: Option<String>,
}

impl FileItem {
    fn expected(&self) -> bool {
        matches!(
            self.kind,
            FileKind::ExpectedSummary | FileKind::ExpectedFile
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Abort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Files,
    Network,
}

pub struct Review {
    pub files: Vec<FileItem>,
    pub network: Vec<NetworkContact>,
    /// Banner instead of contacts when the capture was unavailable/unreadable.
    pub network_banner: Option<String>,
    pub exit_code: i32,
    pub command: String,
}

struct App {
    review: Review,
    tab: Tab,
    file_state: ListState,
    net_state: ListState,
    diff_scroll: u16,
}

/// Full-screen review; returns the user's decision. The interactive attach has
/// already restored cooked mode, but raw mode toggling is idempotent so we
/// disable defensively first.
pub fn run(review: Review) -> Result<Decision> {
    let _ = crossterm::terminal::disable_raw_mode();
    let mut terminal = ratatui::init();
    let mut app = App {
        review,
        tab: Tab::Files,
        file_state: ListState::default(),
        net_state: ListState::default(),
        diff_scroll: 0,
    };
    if !app.review.files.is_empty() {
        app.file_state.select(Some(0));
    }
    if !app.review.network.is_empty() {
        app.net_state.select(Some(0));
    }

    let decision = loop {
        terminal.draw(|f| draw(f, &mut app))?;
        if !crossterm::event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('a'), _) => break Decision::Approve,
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => break Decision::Abort,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => break Decision::Abort,
            (KeyCode::Tab, _) => {
                app.tab = match app.tab {
                    Tab::Files => Tab::Network,
                    Tab::Network => Tab::Files,
                };
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                app.select(1);
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                app.select(-1);
            }
            (KeyCode::PageDown, _) => app.diff_scroll = app.diff_scroll.saturating_add(20),
            (KeyCode::PageUp, _) => app.diff_scroll = app.diff_scroll.saturating_sub(20),
            _ => {}
        }
    };

    ratatui::restore();
    Ok(decision)
}

impl App {
    fn select(&mut self, delta: i64) {
        let (state, len) = match self.tab {
            Tab::Files => (&mut self.file_state, self.review.files.len()),
            Tab::Network => (&mut self.net_state, self.review.network.len()),
        };
        if len == 0 {
            return;
        }
        let current = state.selected().unwrap_or(0) as i64;
        let next = (current + delta).clamp(0, len as i64 - 1) as usize;
        state.select(Some(next));
        if matches!(self.tab, Tab::Files) {
            self.diff_scroll = 0;
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    match app.tab {
        Tab::Files => draw_files(f, app, chunks[1]),
        Tab::Network => draw_network(f, app, chunks[1]),
    }

    let hints = " ↑↓/jk select   Tab switch tab   PgUp/PgDn scroll diff   a approve   q abort ";
    f.render_widget(
        Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let unexpected_files = app.review.files.iter().filter(|i| !i.expected()).count();
    let unexpected_net = app.review.network.iter().filter(|c| !c.known).count();

    let mut spans = vec![Span::styled(
        format!(" {} ", app.review.command),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if app.review.exit_code == 0 {
        spans.push(Span::styled(" exit: 0 ", Style::default().fg(Color::Green)));
    } else {
        spans.push(Span::styled(
            format!(" exit: {} ", app.review.exit_code),
            Style::default().fg(Color::White).bg(Color::Red),
        ));
    }
    spans.push(Span::raw("  "));
    for (tab, label, badge) in [
        (Tab::Files, "Files", unexpected_files),
        (Tab::Network, "Network", unexpected_net),
    ] {
        let style = if app.tab == tab {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let text = if badge > 0 {
            format!(" {label} ({badge} unexpected) ")
        } else {
            format!(" {label} ")
        };
        spans.push(Span::styled(text, style));
        spans.push(Span::raw(" "));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" boxme review "),
        ),
        area,
    );
}

fn file_style(kind: &FileKind) -> Style {
    match kind {
        FileKind::ExpectedSummary | FileKind::ExpectedFile => Style::default().fg(Color::Green),
        FileKind::Added | FileKind::Modified | FileKind::Binary => {
            Style::default().fg(Color::Yellow)
        }
        FileKind::Deleted => Style::default().fg(Color::Red),
    }
}

fn draw_files(f: &mut Frame, app: &mut App, area: Rect) {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    let items: Vec<ListItem> = app
        .review
        .files
        .iter()
        .map(|item| {
            let prefix = match item.kind {
                FileKind::ExpectedSummary => "  ",
                FileKind::ExpectedFile => "~ ",
                FileKind::Added => "+ ",
                FileKind::Modified => "~ ",
                FileKind::Deleted => "- ",
                FileKind::Binary => "* ",
            };
            ListItem::new(format!("{prefix}{}", item.label)).style(file_style(&item.kind))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" changes "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, halves[0], &mut app.file_state);

    let selected = app
        .file_state
        .selected()
        .and_then(|i| app.review.files.get(i));
    let diff_text = match selected {
        Some(item) => match &item.diff {
            Some(diff) => colorize_diff(diff),
            None => vec![Line::from(Span::styled(
                match item.kind {
                    FileKind::ExpectedSummary => "expected write-set — contents not itemized",
                    FileKind::Binary => "binary file — no diff",
                    FileKind::Deleted => "deleted",
                    _ => "no diff available",
                },
                Style::default().fg(Color::DarkGray),
            ))],
        },
        None => vec![Line::from("no changes")],
    };

    let diff = Paragraph::new(diff_text)
        .block(Block::default().borders(Borders::ALL).title(" diff "))
        .wrap(Wrap { trim: false })
        .scroll((app.diff_scroll, 0));
    f.render_widget(diff, halves[1]);
}

fn colorize_diff(diff: &str) -> Vec<Line<'_>> {
    diff.lines()
        .map(|line| {
            let style = if line.starts_with('+') && !line.starts_with("+++") {
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') && !line.starts_with("---") {
                Style::default().fg(Color::Red)
            } else if line.starts_with("@@") {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            Line::from(Span::styled(line, style))
        })
        .collect()
}

fn draw_network(f: &mut Frame, app: &mut App, area: Rect) {
    if let Some(banner) = &app.review.network_banner {
        f.render_widget(
            Paragraph::new(banner.as_str())
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title(" network "))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .review
        .network
        .iter()
        .map(|c| {
            let label = match &c.domain {
                Some(domain) => format!("{domain} ({}) :{}", c.ip, c.port),
                None => format!("{} :{}", c.ip, c.port),
            };
            let (tag, style) = if c.known {
                ("known      ", Style::default().fg(Color::Green))
            } else {
                ("unexpected ", Style::default().fg(Color::Yellow))
            };
            ListItem::new(format!("{tag}{label}")).style(style)
        })
        .collect();

    let title = format!(" network contacts ({}) ", app.review.network.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut app.net_state);
}
