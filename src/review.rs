use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::netcap::NetworkContact;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// One network row in the review. In a learn run the unexpected, named hosts are
/// `selectable` and toggling `selected` adds them to the allowlist.
#[derive(Debug, Clone)]
pub struct NetRow {
    pub contact: NetworkContact,
    pub selectable: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Abort,
}

/// What the review returns: the decision plus, for a learn run, the hosts the
/// user chose to trust.
pub struct Outcome {
    pub decision: Decision,
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Files,
    Network,
}

pub struct Review {
    pub files: Vec<FileItem>,
    pub network: Vec<NetRow>,
    /// Learn run: the Network tab shows checkboxes and `Space` trusts a host.
    pub network_selectable: bool,
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
    /// Inner height of the body pane, captured during draw so paging keys
    /// match the actual viewport.
    page_height: u16,
    /// Wrapped line count of the current diff, captured during draw so
    /// scrolling clamps at the end.
    diff_lines: u16,
}

/// Full-screen review; returns the user's decision (and trusted hosts). The
/// interactive attach has already restored cooked mode, but raw mode toggling
/// is idempotent so we disable defensively first.
pub fn run(review: Review) -> Result<Outcome> {
    let _ = crossterm::terminal::disable_raw_mode();
    let mut app = App {
        review,
        tab: Tab::Files,
        file_state: ListState::default(),
        net_state: ListState::default(),
        diff_scroll: 0,
        page_height: 0,
        diff_lines: 0,
    };
    if !app.review.files.is_empty() {
        app.file_state.select(Some(0));
    }
    if !app.review.network.is_empty() {
        app.net_state.select(Some(0));
    }

    // Restore the terminal whether the loop returns a decision or an error — an
    // error mid-draw must not strand the user in raw mode on the alt screen.
    let mut terminal = ratatui::init();
    let decision = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    let decision = decision?;

    let allow = if decision == Decision::Approve {
        app.review
            .network
            .iter()
            .filter(|r| r.selectable && r.selected)
            .filter_map(|r| r.contact.domain.clone())
            .collect()
    } else {
        Vec::new()
    };

    Ok(Outcome { decision, allow })
}

/// The draw/input loop, split out so `run` can restore the terminal on the
/// error path as well as the normal one.
fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<Decision> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        // Block until input arrives; nothing in the review animates, so there's
        // no reason to wake on a timer.
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Esc is deliberately unbound: vim users press it reflexively, and a
        // single keystroke must not abort a run that took minutes to produce.
        match (key.code, key.modifiers) {
            (KeyCode::Char('a'), KeyModifiers::NONE) => return Ok(Decision::Approve),
            (KeyCode::Char('q'), KeyModifiers::NONE) => return Ok(Decision::Abort),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(Decision::Abort),
            (KeyCode::Tab, _)
            | (KeyCode::BackTab, _)
            | (KeyCode::Char('h'), KeyModifiers::NONE)
            | (KeyCode::Char('l'), KeyModifiers::NONE) => app.switch_tab(),
            (KeyCode::Char('1'), KeyModifiers::NONE) => app.tab = Tab::Files,
            (KeyCode::Char('2'), KeyModifiers::NONE) => app.tab = Tab::Network,
            (KeyCode::Char(' '), _) if matches!(app.tab, Tab::Network) => app.toggle_net(),
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => app.select(1),
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => app.select(-1),
            (KeyCode::Char('g'), KeyModifiers::NONE) => app.select_index(0),
            (KeyCode::Char('G'), _) => app.select_index(usize::MAX),
            (KeyCode::Char('J'), _) => app.page(1),
            (KeyCode::Char('K'), _) => app.page(-1),
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => app.page(app.half_page()),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => app.page(-app.half_page()),
            (KeyCode::Char('f'), KeyModifiers::CONTROL) | (KeyCode::PageDown, _) => {
                app.page(app.full_page())
            }
            (KeyCode::Char('b'), KeyModifiers::CONTROL) | (KeyCode::PageUp, _) => {
                app.page(-app.full_page())
            }
            _ => {}
        }
    }
}

impl App {
    fn list_parts(&mut self) -> (&mut ListState, usize) {
        match self.tab {
            Tab::Files => (&mut self.file_state, self.review.files.len()),
            Tab::Network => (&mut self.net_state, self.review.network.len()),
        }
    }

    fn select(&mut self, delta: i64) {
        let (state, len) = self.list_parts();
        if len == 0 {
            return;
        }
        let current = state.selected().unwrap_or(0) as i64;
        let next = (current + delta).clamp(0, len as i64 - 1) as usize;
        self.select_index(next);
    }

    fn select_index(&mut self, index: usize) {
        let (state, len) = self.list_parts();
        if len == 0 {
            return;
        }
        state.select(Some(index.min(len - 1)));
        if matches!(self.tab, Tab::Files) {
            self.diff_scroll = 0;
        }
    }

    fn switch_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Files => Tab::Network,
            Tab::Network => Tab::Files,
        };
    }

    /// Page keys act on whichever pane matters for the current tab: the diff
    /// on Files, the list itself on Network.
    fn page(&mut self, delta: i32) {
        match self.tab {
            Tab::Files => {
                let max = self.diff_lines.saturating_sub(self.page_height) as i32;
                self.diff_scroll = (i32::from(self.diff_scroll) + delta).clamp(0, max) as u16;
            }
            Tab::Network => self.select(delta as i64),
        }
    }

    fn half_page(&self) -> i32 {
        i32::from((self.page_height / 2).max(1))
    }

    fn full_page(&self) -> i32 {
        i32::from(self.page_height.max(1))
    }

    fn toggle_net(&mut self) {
        if !self.review.network_selectable {
            return;
        }
        if let Some(row) = self
            .net_state
            .selected()
            .and_then(|i| self.review.network.get_mut(i))
        {
            if row.selectable {
                row.selected = !row.selected;
            }
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
    app.page_height = chunks[1].height.saturating_sub(2);
    match app.tab {
        Tab::Files => draw_files(f, app, chunks[1]),
        Tab::Network => draw_network(f, app, chunks[1]),
    }

    let hints = match app.tab {
        Tab::Files => {
            " ↑↓/jk select   g/G first/last   ^d/^u scroll diff   h/l tab   a approve   q abort "
        }
        Tab::Network if app.review.network_selectable => {
            " ↑↓/jk select   Space trust host   h/l tab   a approve   q abort "
        }
        Tab::Network => " ↑↓/jk select   g/G first/last   h/l tab   a approve   q abort ",
    };
    f.render_widget(
        Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let unexpected_files = app.review.files.iter().filter(|i| !i.expected()).count();
    let unexpected_net = app
        .review
        .network
        .iter()
        .filter(|r| !r.contact.known)
        .count();

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

fn file_style(kind: FileKind) -> Style {
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
            ListItem::new(format!("{prefix}{}", item.label)).style(file_style(item.kind))
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

    // Wrapped-line estimate so scrolling clamps at the end of the diff.
    let inner_width = usize::from(halves[1].width.saturating_sub(2)).max(1);
    let total: usize = diff_text
        .iter()
        .map(|line| line.width().div_ceil(inner_width).max(1))
        .sum();
    app.diff_lines = total.min(usize::from(u16::MAX)) as u16;
    app.diff_scroll = app
        .diff_scroll
        .min(app.diff_lines.saturating_sub(app.page_height));

    let title = if app.diff_lines > app.page_height {
        let bottom = (app.diff_scroll + app.page_height).min(app.diff_lines);
        format!(" diff {bottom}/{} ", app.diff_lines)
    } else {
        " diff ".to_string()
    };
    let diff = Paragraph::new(diff_text)
        .block(Block::default().borders(Borders::ALL).title(title))
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

    let selectable = app.review.network_selectable;
    let items: Vec<ListItem> = app
        .review
        .network
        .iter()
        .map(|row| {
            let c = &row.contact;
            let label = match &c.domain {
                Some(domain) => format!("{domain} ({}) :{}", c.ip, c.port),
                None => format!("{} :{}", c.ip, c.port),
            };
            // Checkbox only for selectable rows in learn mode; everything else
            // keeps a 4-space gutter so the columns line up.
            let check = if !selectable {
                ""
            } else if !row.selectable {
                "    "
            } else if row.selected {
                "[x] "
            } else {
                "[ ] "
            };
            let (tag, style) = if c.known {
                ("known      ", Style::default().fg(Color::Green))
            } else if selectable && row.selected {
                ("trusted    ", Style::default().fg(Color::Green))
            } else {
                ("unexpected ", Style::default().fg(Color::Yellow))
            };
            ListItem::new(format!("{check}{tag}{label}")).style(style)
        })
        .collect();

    let title = if selectable {
        format!(
            " network contacts ({}) — Space trusts an unexpected host ",
            app.review.network.len()
        )
    } else {
        format!(" network contacts ({}) ", app.review.network.len())
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut app.net_state);
}
