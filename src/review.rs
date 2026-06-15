use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::netcap::NetworkContact;
use crate::outside::{SysFile, SysKind};

/// Cap on how many lines the expanded command view (toggled with `c`) takes, so
/// a pathological command can't swallow the whole header.
const MAX_FULL_CMD_LINES: usize = 6;

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

/// How a contacted host stands relative to the current network policy. Drives
/// the colour and label in the Network tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetStatus {
    /// Built-in package registry — always reachable.
    Known,
    /// Reachable because the project allowlist permits it (enforced run).
    Allowed,
    /// Learn run: contacted with the network open, not yet trusted.
    Observed,
    /// Enforced run: denied by the policy.
    Blocked,
}

/// One network row in the review. A `selectable` row can be toggled with Space:
/// in a learn run that trusts an observed host; in an enforced run it marks a
/// blocked host to allow on a clean re-run.
#[derive(Debug, Clone)]
pub struct NetRow {
    pub contact: NetworkContact,
    pub status: NetStatus,
    pub selectable: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Abort,
    /// Allow the marked blocked hosts and re-run under enforcement.
    Rerun,
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
    Outside,
}

pub struct Review {
    pub files: Vec<FileItem>,
    pub network: Vec<NetRow>,
    /// Learn run: the Network tab shows checkboxes and `Space` trusts a host.
    pub network_selectable: bool,
    /// Enforced run: blocked hosts can be marked and `r` allows them + re-runs.
    pub allow_rerun: bool,
    /// Banner instead of contacts when the capture was unavailable/unreadable.
    pub network_banner: Option<String>,
    /// Paths the command wrote outside /workspace — a supply-chain signal.
    pub outside: Vec<SysFile>,
    /// Banner instead of the list when the out-of-workspace scan couldn't run.
    pub outside_banner: Option<String>,
    /// The scan hit its cap; more paths changed than are shown.
    pub outside_truncated: bool,
    pub exit_code: i32,
    pub command: String,
}

struct App {
    review: Review,
    tab: Tab,
    file_state: ListState,
    net_state: ListState,
    out_state: ListState,
    diff_scroll: u16,
    /// Inner height of the body pane, captured during draw so paging keys
    /// match the actual viewport.
    page_height: u16,
    /// Wrapped line count of the current diff, captured during draw so
    /// scrolling clamps at the end.
    diff_lines: u16,
    /// `c` toggles this to expand a truncated command to its full wrapped form.
    show_full_command: bool,
    /// When set, the allow-and-re-run confirmation popup is shown.
    confirming: bool,
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
        out_state: ListState::default(),
        diff_scroll: 0,
        page_height: 0,
        diff_lines: 0,
        show_full_command: false,
        confirming: false,
    };
    if !app.review.files.is_empty() {
        app.file_state.select(Some(0));
    }
    if !app.review.network.is_empty() {
        app.net_state.select(Some(0));
    }
    if !app.review.outside.is_empty() {
        app.out_state.select(Some(0));
    }

    // Restore the terminal whether the loop returns a decision or an error — an
    // error mid-draw must not strand the user in raw mode on the alt screen.
    let mut terminal = ratatui::init();
    let decision = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    let decision = decision?;

    let allow = match decision {
        Decision::Approve | Decision::Rerun => app
            .review
            .network
            .iter()
            .filter(|r| r.selectable && r.selected)
            .filter_map(|r| r.contact.domain.clone())
            .collect(),
        Decision::Abort => Vec::new(),
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
        // While the confirmation popup is up, only the y/n answer matters.
        if app.confirming {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(Decision::Rerun),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                    app.confirming = false
                }
                _ => {}
            }
            continue;
        }
        // Esc is deliberately unbound: vim users press it reflexively, and a
        // single keystroke must not abort a run that took minutes to produce.
        match (key.code, key.modifiers) {
            (KeyCode::Char('a'), KeyModifiers::NONE) => return Ok(Decision::Approve),
            (KeyCode::Char('q'), KeyModifiers::NONE) => return Ok(Decision::Abort),
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(Decision::Abort),
            (KeyCode::Char('r'), KeyModifiers::NONE) if app.can_rerun() => app.confirming = true,
            (KeyCode::Tab, _)
            | (KeyCode::BackTab, _)
            | (KeyCode::Char('h'), KeyModifiers::NONE)
            | (KeyCode::Char('l'), KeyModifiers::NONE) => app.switch_tab(),
            (KeyCode::Char('1'), KeyModifiers::NONE) => app.tab = Tab::Files,
            (KeyCode::Char('2'), KeyModifiers::NONE) => app.tab = Tab::Network,
            (KeyCode::Char('3'), KeyModifiers::NONE) => app.tab = Tab::Outside,
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                app.show_full_command = !app.show_full_command
            }
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
            Tab::Outside => (&mut self.out_state, self.review.outside.len()),
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
            Tab::Network => Tab::Outside,
            Tab::Outside => Tab::Files,
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
            Tab::Network | Tab::Outside => self.select(delta as i64),
        }
    }

    fn half_page(&self) -> i32 {
        i32::from((self.page_height / 2).max(1))
    }

    fn full_page(&self) -> i32 {
        i32::from(self.page_height.max(1))
    }

    /// Whether `r` can open the confirmation popup: an enforced run with at least
    /// one blocked host marked.
    fn can_rerun(&self) -> bool {
        self.review.allow_rerun
            && self
                .review
                .network
                .iter()
                .any(|r| r.selectable && r.selected)
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
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height(app, area.width)),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, app, chunks[0]);
    app.page_height = chunks[1].height.saturating_sub(2);
    match app.tab {
        Tab::Files => draw_files(f, app, chunks[1]),
        Tab::Network => draw_network(f, app, chunks[1]),
        Tab::Outside => draw_outside(f, app, chunks[1]),
    }

    let hints = match app.tab {
        Tab::Files => {
            " ↑↓/jk select   g/G first/last   ^d/^u scroll diff   h/l tab   c cmd   a approve   q abort "
        }
        Tab::Network if app.review.allow_rerun => {
            " ↑↓/jk select   Space mark blocked   r allow+rerun   h/l tab   a approve   q abort "
        }
        Tab::Network if app.review.network_selectable => {
            " ↑↓/jk select   Space trust host   h/l tab   c cmd   a approve   q abort "
        }
        Tab::Network | Tab::Outside => {
            " ↑↓/jk select   g/G first/last   h/l tab   c cmd   a approve   q abort "
        }
    };
    f.render_widget(
        Paragraph::new(hints).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    if app.confirming {
        draw_confirm(f, app);
    }
}

/// The run status + tabs, always shown. Built separately from the command so
/// the command can be measured against (or moved off) the line they share.
fn status_spans(app: &App) -> Vec<Span<'static>> {
    let unexpected_files = app.review.files.iter().filter(|i| !i.expected()).count();
    let unexpected_net = app
        .review
        .network
        .iter()
        .filter(|r| matches!(r.status, NetStatus::Blocked | NetStatus::Observed))
        .count();
    let outside_count = app.review.outside.len();

    let mut tail: Vec<Span<'static>> = Vec::new();
    if app.review.exit_code == 0 {
        tail.push(Span::styled(" exit: 0 ", Style::default().fg(Color::Green)));
    } else {
        tail.push(Span::styled(
            format!(" exit: {} ", app.review.exit_code),
            Style::default().fg(Color::White).bg(Color::Red),
        ));
    }
    tail.push(Span::raw("  "));
    for (tab, label, badge) in [
        (Tab::Files, "Files", unexpected_files),
        (Tab::Network, "Network", unexpected_net),
        (Tab::Outside, "Outside", outside_count),
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
        tail.push(Span::styled(text, style));
        tail.push(Span::raw(" "));
    }
    tail
}

/// Width left for the command once the status + tabs claim their share of the
/// header's inner width.
fn command_budget(app: &App, width: u16) -> usize {
    let inner = usize::from(width.saturating_sub(2));
    let tail_width: usize = status_spans(app).iter().map(|s| s.width()).sum();
    inner.saturating_sub(tail_width)
}

/// Header rows: 3 normally; taller only when the command is expanded (`c`) *and*
/// is actually too long to fit beside the tabs.
fn header_height(app: &App, width: u16) -> u16 {
    let budget = command_budget(app, width);
    let fits = app.review.command.chars().count() + 2 <= budget;
    if !app.show_full_command || fits {
        return 3;
    }
    let inner = usize::from(width.saturating_sub(2)).max(1);
    let lines = command_lines(&app.review.command, inner).len();
    (lines + 3) as u16
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let tail = status_spans(app);
    let cmd_style = Style::default().add_modifier(Modifier::BOLD);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" boxme review ");

    let budget = command_budget(app, area.width);
    let framed = format!(" {} ", app.review.command);
    let fits = framed.chars().count() <= budget;

    let paragraph = if !app.show_full_command || fits {
        // Single line: command (truncated with … if needed) then status + tabs.
        let cmd = if fits {
            framed
        } else {
            truncate_command(&app.review.command, budget)
        };
        let mut spans = vec![Span::styled(cmd, cmd_style)];
        spans.extend(tail);
        Paragraph::new(Line::from(spans))
    } else {
        // Expanded: status + tabs on top, the full command wrapped below.
        let inner = usize::from(area.width.saturating_sub(2)).max(1);
        let mut lines = vec![Line::from(tail)];
        for piece in command_lines(&app.review.command, inner) {
            lines.push(Line::from(Span::styled(piece, cmd_style)));
        }
        Paragraph::new(lines)
    };

    f.render_widget(paragraph.block(block), area);
}

/// Fit the command into `budget` cells with a trailing ellipsis. Reserves three
/// cells for the leading space, the `…`, and the trailing space.
fn truncate_command(command: &str, budget: usize) -> String {
    let keep = budget.saturating_sub(3);
    let head: String = command.chars().take(keep).collect();
    format!(" {head}… ")
}

/// Split the command into `width`-wide lines for the expanded view, capped at
/// `MAX_FULL_CMD_LINES` with a `…` on the last line when it overflows.
fn command_lines(command: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = command.chars().collect();
    let mut lines: Vec<String> = chars.chunks(width).map(|c| c.iter().collect()).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > MAX_FULL_CMD_LINES {
        lines.truncate(MAX_FULL_CMD_LINES);
        if let Some(last) = lines.last_mut() {
            last.pop();
            last.push('…');
        }
    }
    lines
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
            // Checkbox only when the run permits marking hosts; everything else
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
            let (tag, style) = match row.status {
                NetStatus::Known => ("known      ", Style::default().fg(Color::Green)),
                NetStatus::Allowed => ("allowed    ", Style::default().fg(Color::Green)),
                NetStatus::Observed if row.selected => {
                    ("trusted    ", Style::default().fg(Color::Green))
                }
                NetStatus::Observed => ("unexpected ", Style::default().fg(Color::Yellow)),
                NetStatus::Blocked if row.selected => {
                    ("allow      ", Style::default().fg(Color::Green))
                }
                NetStatus::Blocked => ("blocked    ", Style::default().fg(Color::Yellow)),
            };
            ListItem::new(format!("{check}{tag}{label}")).style(style)
        })
        .collect();

    let title = if app.review.allow_rerun {
        format!(
            " network contacts ({}) — Space marks a blocked host, r allows + re-runs ",
            app.review.network.len()
        )
    } else if selectable {
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

fn draw_outside(f: &mut Frame, app: &mut App, area: Rect) {
    if let Some(banner) = &app.review.outside_banner {
        f.render_widget(
            Paragraph::new(banner.as_str())
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title(" outside "))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    if app.review.outside.is_empty() {
        f.render_widget(
            Paragraph::new("nothing written outside /workspace")
                .style(Style::default().fg(Color::Green))
                .block(Block::default().borders(Borders::ALL).title(" outside ")),
            area,
        );
        return;
    }

    let items: Vec<ListItem> = app
        .review
        .outside
        .iter()
        .map(|file| {
            let tag = match file.kind {
                SysKind::File => "file",
                SysKind::Symlink => "link",
            };
            ListItem::new(format!(
                "{tag}  {:>9}  {}",
                human_size(file.size),
                file.path
            ))
            .style(Style::default().fg(Color::Yellow))
        })
        .collect();

    let count = app.review.outside.len();
    let title = if app.review.outside_truncated {
        format!(" outside /workspace ({count}+, truncated) ")
    } else {
        format!(" outside /workspace ({count}) ")
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut app.out_state);
}

/// The allow-and-re-run confirmation, drawn over the review. Lists exactly which
/// blocked hosts will be written to `.boxme/allow` before the clean re-run.
fn draw_confirm(f: &mut Frame, app: &App) {
    let hosts: Vec<&str> = app
        .review
        .network
        .iter()
        .filter(|r| r.selectable && r.selected)
        .map(|r| r.contact.host())
        .collect();

    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "Allow {} host(s) and re-run under enforcement?",
                hosts.len()
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for host in &hosts {
        lines.push(Line::from(Span::styled(
            format!("  {host}"),
            Style::default().fg(Color::Green),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[y]", Style::default().fg(Color::Green)),
        Span::raw(" allow + re-run    "),
        Span::styled("[n]", Style::default().fg(Color::Red)),
        Span::raw(" cancel"),
    ]));

    let area = centered(f.area(), 64, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" confirm "),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// A `width`×`height` rectangle centered in `area`, clamped to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Compact byte size for the Outside list (e.g. `12.3 KB`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_command_fits_budget_and_marks_ellipsis() {
        let cmd = "composer update some/really-long-package --with=flags";
        let out = truncate_command(cmd, 12);
        assert_eq!(out.chars().count(), 12);
        assert!(out.starts_with(" composer"));
        assert!(out.ends_with("… "));
    }

    #[test]
    fn command_lines_chunks_and_caps_with_ellipsis() {
        assert_eq!(command_lines("composer install", 80), ["composer install"]);

        let long = "x".repeat(1000);
        let lines = command_lines(&long, 10);
        assert_eq!(lines.len(), MAX_FULL_CMD_LINES);
        assert!(lines
            .iter()
            .take(MAX_FULL_CMD_LINES - 1)
            .all(|l| l.len() == 10));
        assert!(lines.last().unwrap().ends_with('…'));
    }
}
